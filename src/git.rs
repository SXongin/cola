//! Best-effort git state capture for the Turn Footer (ADR-0019).

/// The git state of a working directory, shown on the Turn Footer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitState {
    /// Current branch name; the short commit hash when detached.
    pub branch: Option<String>,
    /// Working tree differs from HEAD, including untracked files.
    pub dirty: bool,
    /// The directory is a LINKED git worktree (#433): an extra checkout the
    /// main repository manages (`.git/worktrees/<name>`), not the main
    /// checkout and not a submodule. Only meaningful alongside a branch.
    pub worktree: bool,
}

/// The project name — the basename of a working directory (e.g. "cola" for
/// `/root/workspace/dev/cola`).
pub fn project_name(dir: &str) -> Option<String> {
    std::path::Path::new(dir)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
}

/// Read the git state of `dir`: the current branch (or short commit hash when
/// detached) and whether the working tree is dirty. Strictly best effort — a
/// non-git directory or any git failure yields the default (no branch, clean)
/// so the card footer simply omits the git halves. The halves are omitted
/// together: an empty repo (no HEAD) has a succeeding `status --porcelain` but
/// a failing `rev-parse`, and a failed status read — indistinguishable from a
/// clean tree at the `git()` level — yields no state at all, so a turn-end
/// refresh keeps the start capture instead of clearing its ⚠ as if clean.
pub async fn read_state(dir: &str) -> GitState {
    let branch = match git(dir, &["rev-parse", "--abbrev-ref", "HEAD"]).await {
        Some(branch) if branch != "HEAD" => Some(branch),
        Some(_) => git(dir, &["rev-parse", "--short", "HEAD"]).await,
        None => None,
    };
    let Some(branch) = branch else {
        return GitState::default();
    };
    match status_dirty(dir).await {
        Some(dirty) => GitState {
            branch: Some(branch),
            dirty,
            worktree: linked_worktree(dir).await,
        },
        None => GitState::default(),
    }
}

/// Whether `dir` is a LINKED worktree (#433): its `--git-dir` points into the
/// main repository's `.git/worktrees/<name>` while `--git-common-dir` stays at
/// the main `.git`. A main checkout resolves both to the same directory, but
/// git prints the paths relative to `dir` — a subdirectory of a main checkout
/// yields `/repo/.git` vs `../.git` — so both are resolved against `dir`
/// before comparing. A submodule (its `.git` file redirects into
/// `.git/modules/<name>`) resolves both to the same directory too, so
/// submodules are not flagged.
async fn linked_worktree(dir: &str) -> bool {
    let Some(paths) = git(dir, &["rev-parse", "--git-dir", "--git-common-dir"]).await else {
        return false;
    };
    let mut lines = paths.lines();
    match (lines.next(), lines.next()) {
        (Some(git_dir), Some(common_dir)) => {
            resolve_git_path(dir, git_dir) != resolve_git_path(dir, common_dir)
        }
        _ => false,
    }
}

/// Resolve one of `git rev-parse`'s relative-to-`dir` paths into an absolute
/// one, canonicalized so `.`/`..` spellings and symlinks compare equal. The
/// path exists whenever git printed it; when canonicalization fails anyway,
/// the joined path still normalizes the common cases.
fn resolve_git_path(dir: &str, path: &str) -> std::path::PathBuf {
    let joined = if std::path::Path::new(path).is_absolute() {
        std::path::PathBuf::from(path)
    } else {
        std::path::Path::new(dir).join(path)
    };
    std::fs::canonicalize(&joined).unwrap_or(joined)
}

/// `git status --porcelain`: `Some(true)` when the working tree has changes,
/// `Some(false)` when the command succeeded with no output (clean), `None`
/// when git is unavailable or the command failed. `git()` cannot be reused
/// here — it maps a clean tree and a failed command to the same `None`.
async fn status_dirty(dir: &str) -> Option<bool> {
    let out = tokio::process::Command::new("git")
        .args(["-C", dir, "status", "--porcelain"])
        // A dropped read (the dirty fan-out's deadline) must not leave a git
        // child behind: `output()` only waits, it does not kill.
        .kill_on_drop(true)
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(!String::from_utf8_lossy(&out.stdout).trim().is_empty())
}

/// Run a git command in `dir`; returns trimmed stdout, or None when git is
/// unavailable, the command fails, or the output is empty (a clean status).
async fn git(dir: &str, args: &[&str]) -> Option<String> {
    let text = git_capture(dir, args).await.ok()?;
    if text.is_empty() { None } else { Some(text) }
}

/// One checkout `git worktree list --porcelain` reports (ADR-0088).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeEntry {
    /// Absolute path to the checkout.
    pub path: String,
    /// The checked-out branch without `refs/heads/`; `None` for a detached
    /// HEAD (a worktree OpenCode's own API creates is always detached).
    pub branch: Option<String>,
    /// The repository's main checkout — porcelain lists it first — rather
    /// than a linked worktree.
    pub main: bool,
}

/// What `/worktree new` created (ADR-0088).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedWorktree {
    pub path: String,
    pub branch: String,
    /// Whether the configured root is covered by the primary checkout's
    /// `.gitignore`. `false` means the main checkout starts reading dirty, and
    /// the command's reply carries a reminder — cola never edits the
    /// user's `.gitignore` (ADR-0088).
    pub root_ignored: bool,
}

/// A worktree failure whose `Display` is the line the user reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeError(String);

impl WorktreeError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl std::fmt::Display for WorktreeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for WorktreeError {}

/// The parent directory of a project's worktrees, resolved against the
/// repository's **primary checkout** (ADR-0088): the session's own directory
/// may itself be a linked worktree, and nesting a worktree inside one is never
/// the intent.
pub async fn worktree_root(
    dir: &str,
    configured: &std::path::Path,
) -> Result<std::path::PathBuf, WorktreeError> {
    if configured.is_absolute() {
        return Ok(configured.to_path_buf());
    }
    let common = git_capture(dir, &["rev-parse", "--git-common-dir"])
        .await
        .map_err(|_| WorktreeError::new(format!("目录 `{dir}` 不是 git 仓库")))?;
    if common.trim().is_empty() {
        return Err(WorktreeError::new(format!("目录 `{dir}` 不是 git 仓库")));
    }
    let common = resolve_git_path(dir, common.trim());
    let primary = common
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| std::path::PathBuf::from(dir));
    Ok(primary.join(configured))
}

/// List every checkout of the repository containing `dir`, main checkout
/// included (ADR-0088). A non-git directory, or a git failure, is an error the
/// command answers with text instead of an empty card.
pub async fn list_worktrees(dir: &str) -> Result<Vec<WorktreeEntry>, WorktreeError> {
    let inside = git_capture(dir, &["rev-parse", "--is-inside-work-tree"])
        .await
        .map(|out| out.trim() == "true")
        .unwrap_or(false);
    if !inside {
        return Err(WorktreeError::new(format!("目录 `{dir}` 不是 git 仓库")));
    }
    let text = git_capture(dir, &["worktree", "list", "--porcelain"])
        .await
        .map_err(|error| WorktreeError::new(format!("读取 worktree 失败：{error}")))?;
    Ok(parse_worktree_porcelain(&text))
}

/// Parse `git worktree list --porcelain`: blank-line-separated blocks of
/// `worktree <path>`, `HEAD <sha>`, `branch refs/heads/<name>`, `bare`,
/// `detached`, `locked`/`prunable` lines. The first block is the main checkout.
fn parse_worktree_porcelain(text: &str) -> Vec<WorktreeEntry> {
    let mut entries: Vec<WorktreeEntry> = Vec::new();
    let mut current: Option<WorktreeEntry> = None;
    for line in text.lines() {
        if line.is_empty() {
            if let Some(entry) = current.take() {
                entries.push(entry);
            }
            continue;
        }
        if let Some(path) = line.strip_prefix("worktree ") {
            if let Some(entry) = current.take() {
                entries.push(entry);
            }
            current = Some(WorktreeEntry {
                path: path.to_string(),
                branch: None,
                main: entries.is_empty(),
            });
        } else if let Some(branch) = line.strip_prefix("branch refs/heads/")
            && let Some(entry) = current.as_mut()
        {
            entry.branch = Some(branch.to_string());
        }
    }
    if let Some(entry) = current {
        entries.push(entry);
    }
    entries
}

/// The bound the ADR promises for the dirty fan-out on the command path.
pub const DIRTY_BUDGET: std::time::Duration = std::time::Duration::from_secs(5);

/// Dirty state per checkout, bounded (ADR-0088): one `git status --porcelain`
/// each, at most four at a time, all under ONE shared deadline. A read that
/// misses it reads `None` — the card renders `?` instead of claiming a clean
/// tree it never measured — while a read that finished keeps its answer.
pub async fn worktree_dirty_until(paths: &[String], deadline: tokio::time::Instant) -> Vec<Option<bool>> {
    use std::sync::Arc;

    const CONCURRENCY: usize = 4;

    let limit = Arc::new(tokio::sync::Semaphore::new(CONCURRENCY));
    let reads = paths.iter().map(|path| {
        let limit = Arc::clone(&limit);
        async move {
            let read = async move {
                let _permit = limit.acquire().await.ok()?;
                status_dirty(path).await
            };
            tokio::time::timeout_at(deadline, read).await.ok().flatten()
        }
    });
    futures_util::future::join_all(reads).await
}

/// The slug a worktree name becomes (ADR-0088): trim, lowercase,
/// whitespace/underscore to `-`, collapse repeats, trim dashes and dots, then
/// validate. Chinese and other non-ASCII input is refused rather than
/// transliterated — a slug must stay typeable on another machine.
pub fn normalize_name(input: &str) -> Result<String, WorktreeError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(WorktreeError::new("名字不能为空"));
    }
    let mut slug = String::with_capacity(trimmed.len());
    let mut last_dash = false;
    for ch in trimmed.chars() {
        let mapped = if ch.is_ascii_alphanumeric() {
            ch.to_ascii_lowercase()
        } else if ch.is_whitespace() || ch == '_' || ch == '-' {
            '-'
        } else if ch == '.' {
            '.'
        } else {
            return Err(WorktreeError::new(format!(
                "名字 `{trimmed}` 只能包含英文、数字、`.`、`-`、`_`（例如 `user-guide`）"
            )));
        };
        if mapped == '-' {
            if last_dash {
                continue;
            }
            last_dash = true;
        } else {
            last_dash = false;
        }
        slug.push(mapped);
    }
    let slug = slug.trim_matches(['-', '.']).to_string();
    if slug.is_empty() || slug.contains("..") {
        return Err(WorktreeError::new(format!(
            "名字 `{trimmed}` 规范化后为空或包含 `..`；请用英文/数字命名，例如 `user-guide`"
        )));
    }
    if slug.len() > 40 {
        return Err(WorktreeError::new(format!(
            "名字太长（{} 个字符，最多 40）；请缩短后再试",
            slug.len()
        )));
    }
    Ok(slug)
}

/// Create a worktree for `name` under `root` (a path relative to the primary
/// checkout or absolute) from `base` (default `HEAD`) and return what was
/// made (ADR-0088). An existing directory gets a `-2`… suffix, the branch
/// following it; an existing branch no checkout holds is attached, while a
/// checked-out one is refused with git's own reason.
pub async fn create_worktree(
    dir: &str,
    root: &std::path::Path,
    name: &str,
    base: Option<&str>,
) -> Result<CreatedWorktree, WorktreeError> {
    let slug = normalize_name(name)?;
    let bare = git_capture(dir, &["rev-parse", "--is-bare-repository"])
        .await
        .map(|out| out.trim() == "true")
        .unwrap_or(false);
    if bare {
        return Err(WorktreeError::new("这是 bare 仓库，不能创建 worktree"));
    }
    let root = worktree_root(dir, root).await?;
    std::fs::create_dir_all(&root)
        .map_err(|error| WorktreeError::new(format!("无法创建目录 `{}`：{error}", root.display())))?;

    let mut candidate = slug.clone();
    let mut suffix = 1;
    let mut directory = root.join(&candidate);
    while directory.exists() {
        suffix += 1;
        if suffix > 10 {
            return Err(WorktreeError::new(format!(
                "目录 `{}` 及随后的 9 个后缀都已存在",
                root.join(slug).display()
            )));
        }
        candidate = format!("{slug}-{suffix}");
        directory = root.join(&candidate);
    }
    let branch = format!("wt/{candidate}");
    let directory_text = directory.to_string_lossy().to_string();

    let branch_exists = git_capture(
        dir,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )
    .await
    .is_ok();
    if branch_exists {
        let held = list_worktrees(dir)
            .await?
            .into_iter()
            .find(|entry| entry.branch.as_deref() == Some(branch.as_str()));
        if let Some(held) = held {
            return Err(WorktreeError::new(format!(
                "分支 `{branch}` 已在 `{}` 中检出；请换一个名字，或先在那里处理",
                held.path
            )));
        }
        git_capture(dir, &["worktree", "add", &directory_text, &branch])
            .await
            .map_err(|error| WorktreeError::new(format!("git worktree add 失败：{error}")))?;
    } else {
        let start = base.unwrap_or("HEAD");
        git_capture(dir, &["worktree", "add", "-b", &branch, &directory_text, start])
            .await
            .map_err(|error| WorktreeError::new(format!("git worktree add 失败：{error}")))?;
    }

    let root_ignored = root_is_ignored(dir, &root).await;
    Ok(CreatedWorktree {
        path: directory_text,
        branch,
        root_ignored,
    })
}

/// Whether `root` is ignored by the repository at `dir`. A root outside the
/// primary checkout cannot dirty it, so it counts as covered.
async fn root_is_ignored(dir: &str, root: &std::path::Path) -> bool {
    let Ok(common) = git_capture(dir, &["rev-parse", "--git-common-dir"]).await else {
        return true;
    };
    let common = resolve_git_path(dir, common.trim());
    let Some(primary) = common.parent() else {
        return true;
    };
    let Ok(relative) = root
        .canonicalize()
        .unwrap_or_else(|_| root.to_path_buf())
        .strip_prefix(primary.canonicalize().unwrap_or_else(|_| primary.to_path_buf()))
        .map(std::path::Path::to_path_buf)
    else {
        return true;
    };
    if relative.as_os_str().is_empty() {
        return true;
    }
    let output = tokio::process::Command::new("git")
        .args(["-C"])
        .arg(primary)
        .args(["check-ignore", "-q", "--"])
        .arg(&relative)
        .output()
        .await;
    // Exit 0 = ignored; exit 1 = not ignored; anything else (a git error) is
    // not evidence of a dirty main checkout, so it never warns.
    matches!(output, Ok(out) if out.status.code() == Some(0))
}

/// Run a git command and return its trimmed stdout, or the trimmed stderr as
/// the error text — the command layer needs git's own words for a refusal
/// (`branch is already checked out`), which [`git`] discards by design.
async fn git_capture(dir: &str, args: &[&str]) -> Result<String, String> {
    let out = tokio::process::Command::new("git")
        .args(["-C", dir])
        .args(args)
        // Same reason as `status_dirty`: a dropped future must not orphan git.
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|error| format!("git 无法执行：{error}"))?;
    let text = if out.status.success() {
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let stderr = if stderr.is_empty() {
            format!("git 退出码 {}", out.status.code().unwrap_or(-1))
        } else {
            stderr
        };
        return Err(stderr);
    };
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::process::Command;

    fn run(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(["-C"])
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn temp_dir(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("cola-{}-{}", label, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn project_name_takes_basename() {
        assert_eq!(project_name("/root/workspace/dev/cola").as_deref(), Some("cola"));
        assert_eq!(project_name("").as_deref(), None);
        assert_eq!(project_name("repo/").as_deref(), Some("repo"));
    }

    #[tokio::test]
    async fn read_state_reads_branch_and_dirty() {
        let dir = temp_dir("git-test");
        run(&dir, &["init", "-b", "main"]);
        run(&dir, &["config", "user.email", "test@example.com"]);
        run(&dir, &["config", "user.name", "test"]);
        std::fs::write(dir.join("a.txt"), "hello").unwrap();
        run(&dir, &["add", "a.txt"]);
        run(&dir, &["commit", "-m", "init"]);

        let s = dir.to_string_lossy().to_string();
        let clean = read_state(&s).await;
        assert_eq!(clean.branch.as_deref(), Some("main"));
        assert!(!clean.dirty);

        std::fs::write(dir.join("a.txt"), "changed").unwrap();
        let dirty = read_state(&s).await;
        assert!(dirty.dirty);

        std::fs::write(dir.join("untracked.txt"), "new").unwrap();
        let dirty2 = read_state(&s).await;
        assert!(dirty2.dirty, "untracked files count as dirty");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn detached_head_falls_back_to_short_sha() {
        let dir = temp_dir("git-detached");
        run(&dir, &["init", "-b", "main"]);
        run(&dir, &["config", "user.email", "test@example.com"]);
        run(&dir, &["config", "user.name", "test"]);
        std::fs::write(dir.join("a.txt"), "hello").unwrap();
        run(&dir, &["add", "a.txt"]);
        run(&dir, &["commit", "-m", "init"]);
        run(&dir, &["checkout", "--detach"]);

        let s = dir.to_string_lossy().to_string();
        let state = read_state(&s).await;
        assert!(
            state.branch.is_some(),
            "detached HEAD should fall back to short sha"
        );
        assert!(state.branch.as_deref().unwrap().len() >= 7);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #433: a linked worktree is flagged — `--git-dir` points into the main
    /// repository's `.git/worktrees/<name>` while `--git-common-dir` stays the
    /// main `.git`. A main checkout (toplevel or subdirectory, where git
    /// spells the two paths differently) and a submodule are not flagged.
    #[tokio::test]
    async fn read_state_flags_a_linked_worktree_only() {
        let main = temp_dir("git-wt-main");
        run(&main, &["init", "-b", "main"]);
        run(&main, &["config", "user.email", "test@example.com"]);
        run(&main, &["config", "user.name", "test"]);
        std::fs::write(main.join("a.txt"), "hello").unwrap();
        run(&main, &["add", "a.txt"]);
        run(&main, &["commit", "-m", "init"]);

        let root = read_state(&main.to_string_lossy()).await;
        assert!(!root.worktree, "a main checkout is not a worktree");
        assert_eq!(root.branch.as_deref(), Some("main"));

        // A subdirectory of the main checkout: git prints `--git-dir`
        // absolute and `--git-common-dir` relative to the cwd — two spellings
        // of one directory, so the raw strings must not be compared.
        let sub = main.join("sub");
        std::fs::create_dir(&sub).unwrap();
        let sub_state = read_state(&sub.to_string_lossy()).await;
        assert!(
            !sub_state.worktree,
            "a subdirectory of the main checkout is not a worktree"
        );

        // The linked worktree.
        let base = temp_dir("git-wt-base");
        let wt = base.join("zh-user-guide");
        run(&main, &["worktree", "add", "-b", "docs/zh", wt.to_str().unwrap()]);
        let wt_state = read_state(&wt.to_string_lossy()).await;
        assert!(wt_state.worktree, "a linked worktree is flagged");
        assert_eq!(wt_state.branch.as_deref(), Some("docs/zh"));

        // A submodule: its `.git` file redirects into `.git/modules/<name>`,
        // the same directory for both rev-parse outputs — not flagged.
        let sub_repo = temp_dir("git-wt-subrepo");
        run(&sub_repo, &["init", "-b", "main"]);
        run(&sub_repo, &["config", "user.email", "test@example.com"]);
        run(&sub_repo, &["config", "user.name", "test"]);
        std::fs::write(sub_repo.join("b.txt"), "hi").unwrap();
        run(&sub_repo, &["add", "b.txt"]);
        run(&sub_repo, &["commit", "-m", "init"]);
        run(
            &main,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                sub_repo.to_str().unwrap(),
                "mod",
            ],
        );
        let mod_state = read_state(&main.join("mod").to_string_lossy()).await;
        assert!(
            mod_state.branch.is_some(),
            "the submodule read resolves its branch"
        );
        assert!(!mod_state.worktree, "a submodule is not a worktree");

        let _ = std::fs::remove_dir_all(&main);
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&sub_repo);
    }

    #[tokio::test]
    async fn non_git_dir_is_default() {
        let dir = temp_dir("git-nongit");
        let s = dir.to_string_lossy().to_string();
        assert_eq!(read_state(&s).await, GitState::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An empty repo has no HEAD: `rev-parse` fails while `status --porcelain`
    /// succeeds. The dirty flag must not produce a lone ⚠ with no branch
    /// (ADR-0019: the halves are omitted together).
    #[tokio::test]
    async fn empty_repo_drops_dirty_without_branch() {
        let dir = temp_dir("git-empty");
        run(&dir, &["init", "-b", "main"]);
        std::fs::write(dir.join("untracked.txt"), "new").unwrap();

        let s = dir.to_string_lossy().to_string();
        assert_eq!(read_state(&s).await, GitState::default());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A failed `status --porcelain` must not be mistaken for a clean tree:
    /// with the index broken, `rev-parse` still resolves the branch but
    /// `status` fails — the read must yield no state, so a turn-end refresh
    /// keeps the start capture instead of clearing its ⚠ (ADR-0019).
    #[tokio::test]
    async fn failed_status_read_yields_no_state() {
        let dir = temp_dir("git-status-fail");
        run(&dir, &["init", "-b", "main"]);
        run(&dir, &["config", "user.email", "test@example.com"]);
        run(&dir, &["config", "user.name", "test"]);
        std::fs::write(dir.join("a.txt"), "hello").unwrap();
        run(&dir, &["add", "a.txt"]);
        run(&dir, &["commit", "-m", "init"]);

        // Break the index (a directory cannot be mapped as the index file):
        // `rev-parse` reads HEAD and succeeds, `status` fails.
        let index = dir.join(".git/index");
        std::fs::remove_file(&index).unwrap();
        std::fs::create_dir(&index).unwrap();

        let s = dir.to_string_lossy().to_string();
        assert_eq!(read_state(&s).await, GitState::default());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A committed repository with `user.name`/`user.email` configured — the
    /// base every worktree test needs.
    fn init_repo(label: &str) -> std::path::PathBuf {
        let dir = temp_dir(label);
        run(&dir, &["init", "-b", "main"]);
        run(&dir, &["config", "user.email", "test@example.com"]);
        run(&dir, &["config", "user.name", "test"]);
        std::fs::write(dir.join("a.txt"), "hello").unwrap();
        run(&dir, &["add", "a.txt"]);
        run(&dir, &["commit", "-m", "init"]);
        dir
    }

    #[test]
    fn normalize_name_lowercases_collapses_and_trims() {
        assert_eq!(normalize_name("  Fix  Login ").unwrap(), "fix-login");
        assert_eq!(normalize_name("fix_login").unwrap(), "fix-login");
        assert_eq!(normalize_name("a--b").unwrap(), "a-b");
        assert_eq!(normalize_name(".hidden.").unwrap(), "hidden");
        assert_eq!(normalize_name("v1.2").unwrap(), "v1.2");
    }

    #[test]
    fn normalize_name_refuses_chinese_and_long_names() {
        let chinese = normalize_name("用户指南").unwrap_err().to_string();
        assert!(chinese.contains("只能包含英文"), "{chinese}");
        let long = "a".repeat(41);
        assert!(normalize_name(&long).unwrap_err().to_string().contains("太长"));
        assert!(normalize_name("   ").is_err(), "an empty name is refused");
    }

    /// `worktree list` parses the main checkout first, then each linked one
    /// with its branch — and marks the main checkout so the card can label it.
    #[tokio::test]
    async fn worktree_list_marks_main_and_carries_branches() {
        let dir = init_repo("git-wt-list");
        let s = dir.to_string_lossy().to_string();
        let created = create_worktree(&s, Path::new(".worktrees"), "Feature One", None)
            .await
            .unwrap();
        assert_eq!(created.branch, "wt/feature-one");
        assert!(created.path.ends_with(".worktrees/feature-one"));
        assert!(!created.root_ignored, "no .gitignore, so the root is not covered");

        let entries = list_worktrees(&s).await.unwrap();
        assert_eq!(entries.len(), 2, "{entries:?}");
        assert!(entries[0].main, "the main checkout leads");
        assert_eq!(entries[0].branch.as_deref(), Some("main"));
        assert!(!entries[1].main);
        assert_eq!(entries[1].branch.as_deref(), Some("wt/feature-one"));
        assert!(entries[1].path.ends_with(".worktrees/feature-one"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Dirty state is per checkout: a file written in the worktree must not
    /// dirty the main checkout, and the read reports each separately.
    #[tokio::test]
    async fn worktree_dirty_reads_each_checkout_separately() {
        let dir = init_repo("git-wt-dirty");
        let s = dir.to_string_lossy().to_string();
        let created = create_worktree(&s, Path::new(".worktrees"), "dirty", None)
            .await
            .unwrap();
        std::fs::write(Path::new(&created.path).join("new.txt"), "x").unwrap();

        let entries = list_worktrees(&s).await.unwrap();
        let paths: Vec<String> = entries.iter().map(|e| e.path.clone()).collect();
        let dirty = worktree_dirty_until(&paths, tokio::time::Instant::now() + DIRTY_BUDGET).await;
        assert_eq!(
            dirty,
            vec![Some(true), Some(true)],
            "the root is untracked in main too"
        );

        std::fs::write(dir.join(".gitignore"), ".worktrees/\n").unwrap();
        run(&dir, &["add", ".gitignore"]);
        run(&dir, &["commit", "-m", "ignore worktrees"]);
        let paths: Vec<String> = list_worktrees(&s)
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.path)
            .collect();
        assert_eq!(
            worktree_dirty_until(&paths, tokio::time::Instant::now() + DIRTY_BUDGET).await,
            vec![Some(false), Some(true)],
            "ignored root: only the worktree itself is dirty"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The root resolves against the PRIMARY checkout even when the session is
    /// already inside a linked worktree (ADR-0088): never worktree-in-worktree.
    #[tokio::test]
    async fn worktree_root_resolves_against_the_primary_checkout() {
        let dir = init_repo("git-wt-root");
        let s = dir.to_string_lossy().to_string();
        let created = create_worktree(&s, Path::new(".worktrees"), "inside", None)
            .await
            .unwrap();

        let root = worktree_root(&created.path, Path::new(".worktrees"))
            .await
            .unwrap();
        assert_eq!(
            root,
            std::fs::canonicalize(&dir).unwrap().join(".worktrees"),
            "the root must not nest inside the linked worktree"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An existing directory gets the next free `-2` suffix, and the branch
    /// follows it, so the pair never disagrees.
    #[tokio::test]
    async fn worktree_create_suffixes_an_existing_directory() {
        let dir = init_repo("git-wt-suffix");
        let s = dir.to_string_lossy().to_string();
        std::fs::create_dir_all(dir.join(".worktrees").join("taken")).unwrap();

        let created = create_worktree(&s, Path::new(".worktrees"), "taken", None)
            .await
            .unwrap();
        assert!(created.path.ends_with(".worktrees/taken-2"), "{}", created.path);
        assert_eq!(created.branch, "wt/taken-2");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An existing branch that no checkout holds is attached: "open that
    /// branch in a new directory" is the natural intent (ADR-0088).
    #[tokio::test]
    async fn worktree_create_attaches_an_unheld_branch() {
        let dir = init_repo("git-wt-reuse");
        run(&dir, &["branch", "wt/reuse"]);
        let s = dir.to_string_lossy().to_string();

        let created = create_worktree(&s, Path::new(".worktrees"), "reuse", None)
            .await
            .unwrap();
        assert_eq!(created.branch, "wt/reuse");
        let inside = std::process::Command::new("git")
            .args(["-C", &created.path, "rev-parse", "--abbrev-ref", "HEAD"])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&inside.stdout).trim(), "wt/reuse");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A branch already checked out somewhere is refused with git's own
    /// reason instead of a second checkout.
    #[tokio::test]
    async fn worktree_create_refuses_a_checked_out_branch() {
        let dir = init_repo("git-wt-held");
        run(&dir, &["branch", "wt/held"]);
        run(&dir, &["worktree", "add", ".worktrees/other", "wt/held"]);
        let s = dir.to_string_lossy().to_string();

        let error = create_worktree(&s, Path::new(".worktrees"), "held", None)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("已在"), "{error}");
        assert!(error.contains("wt/held"), "{error}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A deadline that has already passed reads every checkout as unknown
    /// instead of claiming a clean tree — and returns without waiting.
    #[tokio::test]
    async fn worktree_dirty_past_deadline_reads_unknown() {
        let dir = init_repo("git-wt-past-deadline");
        let s = dir.to_string_lossy().to_string();
        let paths = vec![s];
        let results = worktree_dirty_until(&paths, tokio::time::Instant::now()).await;
        assert_eq!(results, vec![None]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A non-git directory is an error the card turns into text, never an
    /// empty list.
    #[tokio::test]
    async fn worktree_list_refuses_a_non_git_directory() {
        let dir = temp_dir("git-wt-nongit");
        let s = dir.to_string_lossy().to_string();
        let error = list_worktrees(&s).await.unwrap_err().to_string();
        assert!(error.contains("不是 git 仓库"), "{error}");
        assert!(
            create_worktree(&s, Path::new(".worktrees"), "x", None)
                .await
                .is_err()
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A root covered by `.gitignore` is reported as such, so the reply stays
    /// quiet; cola never edits the file itself.
    #[tokio::test]
    async fn worktree_create_reports_an_ignored_root() {
        let dir = init_repo("git-wt-ignored");
        std::fs::write(dir.join(".gitignore"), ".worktrees/\n").unwrap();
        run(&dir, &["add", ".gitignore"]);
        run(&dir, &["commit", "-m", "ignore"]);
        let s = dir.to_string_lossy().to_string();

        let created = create_worktree(&s, Path::new(".worktrees"), "quiet", None)
            .await
            .unwrap();
        assert!(created.root_ignored);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
