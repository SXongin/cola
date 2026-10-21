//! The `/worktree` command family (ADR-0088): create a git worktree and
//! declare the **Pending Session** that starts there — never move a Session.
//!
//! Creation is cola's own git call, not `/api/worktree`: the server's create is
//! hard-wired to `--detach`, so it produces no branch, and its list carries
//! neither branch nor dirty state. The git plumbing lives in [`crate::git`];
//! this module owns the command-shaped glue: the conversation's project
//! directory, the pending declaration, and the reply's wording.

use crate::bridge::handles::CommandHandles;
use crate::config::ThreadKey;

/// The usage line for a bare or malformed `/worktree new` (the name is
/// required — ADR-0088). `/worktree` with no argument opens the card instead.
pub(crate) const WORKTREE_USAGE: &str = "\
用法：`/worktree new <名字> [--base <ref>]`
- 在 `[worktree] root`（默认 `.worktrees`，相对主检出）下创建 worktree，分支为 `wt/<名字>`。
- 起点是当前会话目录的 HEAD；`--base <ref>` 可指定别的提交。
- 名字会规范化为小写 ASCII slug（英文、数字、`.`、`-`、`_`，最多 40 字符）。
- 下一条消息在该目录创建会话；不移动任何现有会话。
- 不带参数发 `/worktree` 查看与切换已有的 worktree。";

/// `/worktree new <name> [--base <ref>]`: create the checkout, declare the
/// Pending Session there (ADR-0041), and answer with what happened. A running
/// Turn is untouched — the next message simply starts in the new directory.
pub(crate) async fn create(
    handles: &CommandHandles,
    thread_key: &ThreadKey,
    message_id: &str,
    name: String,
    base: Option<String>,
) -> crate::error::Result<()> {
    let source = handles.current_project_directory(thread_key).await;
    let root = handles.config.worktree_root().to_path_buf();
    let created = match crate::git::create_worktree(&source, &root, &name, base.as_deref()).await {
        Ok(created) => created,
        Err(error) => {
            handles
                .flow
                .platform
                .reply_text(message_id, &format!("⚠️ /worktree new 失败：{error}"))
                .await?;
            return Ok(());
        }
    };

    // ADR-0041: only the intent is recorded; the conversation's next message
    // creates the Session in the worktree. No server-side move, so an in-flight
    // step is never interrupted (#428).
    let title = std::path::Path::new(&created.path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned());
    let pending = handles
        .flow
        .sessions
        .declare_pending(thread_key, created.path.clone(), title)
        .await?;

    let start = base.as_deref().unwrap_or("HEAD");
    let mut reply = format!(
        "✅ 已创建 worktree `{}`\n分支：`{}`（起点 `{}`）\n下一条消息将在该目录创建会话「{}」。",
        created.path,
        created.branch,
        start,
        pending.title.as_deref().unwrap_or("")
    );
    if !created.root_ignored {
        // ADR-0088: a warning, never an edit to the user's `.gitignore`.
        reply.push_str(&format!(
            "\n⚠️ `{}` 未被 `.gitignore` 忽略，主检出会显示为 dirty。",
            handles.config.worktree_root().display()
        ));
    }
    handles.flow.platform.reply_text(message_id, &reply).await?;
    Ok(())
}
