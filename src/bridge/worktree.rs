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

/// The usage line for a bare or malformed worktree-creating form (ADR-0095).
/// `/worktree` with no argument will report the pending intent (ADR-0095) once
/// the card is retired; until then it still opens the read-only card.
pub(crate) const WORKTREE_USAGE: &str = "\
用法：
`/topic --worktree [--name <标题>] [--slug <路径名>] [--base <ref>]`
`/new --worktree [--name <标题>] [--slug <路径名>] [--base <ref>]`
- 在 `[worktree] root`（默认 `.worktrees`，相对主检出）下创建检出，分支为 `wt/<slug>`。
- 起点按 `[worktree] base` → `origin/HEAD` → `origin/main` → `origin/master` → `main` → `master` 解析；`--base` 覆盖，并在命令时固定成 SHA。
- `--name` 是会话标题（可中文）；`--slug` 是目录/分支名（小写 ASCII），省略时从标题推导，推不出用日期默认值。
- 命令只记录意图，第一条消息才真正创建检出；放弃话题不会留下检出。
- 待建期间用 `/worktree` 查看，`/worktree --slug <名字>` 或 `--base <ref>` 修改。已有检出用 `/run git worktree list` 查看。";

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
    // ADR-0095: the base is the integration line, never the source's HEAD.
    // Resolve it to a SHA now, so the reply names what will actually be built.
    let base = match crate::git::resolve_base(
        &source,
        base.as_deref().or_else(|| handles.config.worktree_base()),
    )
    .await
    {
        Ok(base) => base,
        Err(error) => {
            handles
                .flow
                .platform
                .reply_text(message_id, &format!("⚠️ /worktree new 失败：{error}"))
                .await?;
            return Ok(());
        }
    };
    let created = match crate::git::create_worktree(&source, &root, &name, Some(&base.1)).await {
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

    let start = format!("{} @ {}", base.0, &base.1[..7.min(base.1.len())]);
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

/// The deterministic fallback when neither a slug nor a usable ASCII title is
/// given: a date plus a sequence, e.g. `20261103-1`. Collisions are settled by
/// `create_worktree`'s `-2` suffix (ADR-0095).
pub(crate) fn default_slug(now: chrono::DateTime<chrono::Utc>) -> String {
    format!("{}-1", now.format("%Y%m%d"))
}

/// Resolve a worktree intent at command time (ADR-0095): preconditions (a
/// non-bare git source), the integration base to a SHA, the slug, and the
/// planned target. Nothing is checked out here; `create_worktree` runs at
/// materialisation, so an abandoned pending creates no checkout.
pub(crate) async fn intent(
    source: &str,
    root: &std::path::Path,
    name: Option<&str>,
    slug: Option<&str>,
    base: Option<&str>,
    configured_base: Option<&str>,
) -> Result<crate::bridge::session::WorktreeIntent, crate::git::WorktreeError> {
    crate::git::ensure_creatable(source).await?;
    let (base, base_sha) = crate::git::resolve_base(source, base.or(configured_base)).await?;
    let slug = match slug {
        Some(slug) => crate::git::normalize_name(slug)?,
        None => name
            .and_then(crate::git::derive_slug)
            .unwrap_or_else(|| default_slug(chrono::Utc::now())),
    };
    let root = crate::git::worktree_root(source, root).await?;
    // Check the root is creatable now, so a missing permission is a command
    // refusal rather than a first-message surprise. The root is a directory,
    // not a checkout; lazy creation still owns the checkout itself.
    std::fs::create_dir_all(&root).map_err(|error| {
        crate::git::WorktreeError::new(format!("无法创建目录 `{}`：{error}", root.display()))
    })?;
    let target = crate::git::plain_path(root.join(&slug))
        .to_string_lossy()
        .to_string();
    Ok(crate::bridge::session::WorktreeIntent {
        base,
        base_sha,
        slug,
        target,
        path: None,
    })
}

/// The `/new --worktree` reply: what was recorded and what the first message
/// will create. The topic form shows the same facts on its cover instead.
pub(crate) fn declared_reply(
    title: Option<&str>,
    source: &str,
    intent: &crate::bridge::session::WorktreeIntent,
    root: &std::path::Path,
    root_ignored: bool,
) -> String {
    let title = title.map(|title| format!("「{title}」")).unwrap_or_default();
    let mut reply = format!(
        "✅ 已记下 worktree 意图：`{}`\n分支：`{}` · 起点：`{} @ {}`\n源目录：`{source}`\n下一条消息在该目录创建会话{title}。",
        intent.target,
        intent.branch(),
        intent.base,
        intent.short_sha(),
    );
    if !root_ignored {
        reply.push_str(&format!(
            "\n⚠️ `{}` 未被 `.gitignore` 忽略，主检出会显示为 dirty。",
            root.display()
        ));
    }
    reply
}
