//! Best-effort git state capture for the Turn Footer (ADR-0019).

/// The git state of a working directory, shown on the Turn Footer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitState {
    /// Current branch name; the short commit hash when detached.
    pub branch: Option<String>,
    /// Working tree differs from HEAD, including untracked files.
    pub dirty: bool,
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
/// so the card footer simply omits the git halves. The dirty flag is only
/// reported alongside a resolved branch: an empty repo (no HEAD) has a
/// succeeding `status --porcelain` but a failing `rev-parse`, and a lone ⚠
/// with no branch is exactly the "omit the halves" case the footer must avoid.
pub async fn read_state(dir: &str) -> GitState {
    let branch = match git(dir, &["rev-parse", "--abbrev-ref", "HEAD"]).await {
        Some(branch) if branch != "HEAD" => Some(branch),
        Some(_) => git(dir, &["rev-parse", "--short", "HEAD"]).await,
        None => None,
    };
    let dirty = branch.is_some() && git(dir, &["status", "--porcelain"]).await.is_some();
    GitState { branch, dirty }
}

/// Run a git command in `dir`; returns trimmed stdout, or None when git is
/// unavailable, the command fails, or the output is empty (a clean status).
async fn git(dir: &str, args: &[&str]) -> Option<String> {
    let out = tokio::process::Command::new("git")
        .args(["-C", dir])
        .args(args)
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let text = text.trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
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
}
