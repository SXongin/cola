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

/// The usage line for a malformed `/worktree` form (ADR-0095). Bare
/// `/worktree` reports the pending intent; the read-only inventory card is
/// retired in favour of `/run git worktree list`.
pub(crate) const WORKTREE_USAGE: &str = "\
用法：
`/topic --worktree [--name <标题>] [--slug <路径名>] [--base <ref>]`
`/new --worktree [--name <标题>] [--slug <路径名>] [--base <ref>]`
- 在 `[worktree] root`（默认 `.worktrees`，相对主检出）下创建检出，分支为 `wt/<slug>`。
- 起点按 `[worktree] base` → `origin/HEAD` → `origin/main` → `origin/master` → `main` → `master` 解析；`--base` 覆盖，并在命令时固定成 SHA。
- `--name` 是会话标题（可中文）；`--slug` 是目录/分支名（小写 ASCII），省略时从标题推导，推不出用日期默认值。
- 命令只记录意图，第一条消息才真正创建检出；放弃话题不会留下检出。
- 待建期间用 `/worktree` 查看，`/worktree --slug <名字>` 或 `--base <ref>` 修改。已有检出用 `/dir <路径>` 进入，或 `/run git worktree list` 查看。";

/// `/worktree`: report the pending checkout intent. Outside a pending it
/// answers the usage line — the inventory card is retired, and discovery is
/// `/run git worktree list` (ADR-0095).
pub(crate) async fn status(
    handles: &CommandHandles,
    thread_key: &ThreadKey,
    message_id: &str,
) -> crate::error::Result<()> {
    let pending = {
        let store = handles.flow.sessions.store.lock().await;
        store.pending_for(thread_key).cloned()
    };
    let reply = match pending {
        Some(pending) => match &pending.worktree {
            Some(intent) => status_text(&pending, intent),
            None => format!(
                "当前待建会话没有 worktree 意图（目录 `{}`）。\n- 要带 worktree：用 `/topic --worktree`（新话题）或 `/new --worktree`（当前对话）重新声明。\n- 只想换目录：用 `/dir <路径>`。",
                pending.directory
            ),
        },
        None => "当前没有待建会话。\n- 新建带 worktree 的会话：`/topic --worktree`（新话题）或 `/new --worktree`（当前对话）。\n- 进入已有检出：`/dir <路径>`；查看已有检出：`/run git worktree list`。".to_string(),
    };
    handles.flow.platform.reply_text(message_id, &reply).await?;
    Ok(())
}

/// The status is the cover's facts in text form: target, branch, resolved
/// base, source, and the one-line route to change it.
fn status_text(
    pending: &crate::bridge::session::PendingEntry,
    intent: &crate::bridge::session::WorktreeIntent,
) -> String {
    let title = pending
        .title
        .as_deref()
        .map(|title| format!("\n标题：`{title}`"))
        .unwrap_or_default();
    let created = intent
        .path
        .as_deref()
        .map(|path| format!("\n检出已创建：`{path}`（改 slug/base 不再影响它）"))
        .unwrap_or_default();
    format!(
        "🌲 待建 worktree{title}\n目标：`{}`（若被占用会加后缀）\n分支：`{}` · 起点：`{} @ {}`\n源目录：`{}`{created}\n第一条消息在该目录创建会话。用 `/worktree --slug <名字>` 或 `--base <ref>` 修改。",
        intent.target,
        intent.branch(),
        intent.base,
        intent.short_sha(),
        pending.directory
    )
}

/// `/worktree --slug <slug>` / `--base <ref>`: adjust the pending intent
/// before the first message creates it. Once the checkout exists (a failed
/// session create leaves the pending), the directory is fixed — the command
/// says so instead of pretending a rename would move it.
pub(crate) async fn adjust(
    handles: &CommandHandles,
    thread_key: &ThreadKey,
    message_id: &str,
    slug: Option<String>,
    base: Option<String>,
) -> crate::error::Result<()> {
    let pending = {
        let store = handles.flow.sessions.store.lock().await;
        store.pending_for(thread_key).cloned()
    };
    let Some(pending) = pending else {
        handles
            .flow
            .platform
            .reply_text(
                message_id,
                "当前没有待建会话。\n- 新建带 worktree 的会话：`/topic --worktree`（新话题）或 `/new --worktree`（当前对话）。\n- 进入已有检出：`/dir <路径>`；查看已有检出：`/run git worktree list`。",
            )
            .await?;
        return Ok(());
    };
    let Some(intent) = pending.worktree.clone() else {
        handles
            .flow
            .platform
            .reply_text(
                message_id,
                &format!(
                    "当前待建会话没有 worktree 意图（目录 `{}`）。\n- 要带 worktree：用 `/topic --worktree`（新话题）或 `/new --worktree`（当前对话）重新声明。\n- 只想换目录：用 `/dir <路径>`。",
                    pending.directory
                ),
            )
            .await?;
        return Ok(());
    };
    if let Some(path) = intent.path.as_deref() {
        handles
            .flow
            .platform
            .reply_text(
                message_id,
                &format!(
                    "⚠️ 检出已创建：`{path}`；现在改 slug/base 不会移动它。第一条消息会在这个目录创建会话。"
                ),
            )
            .await?;
        return Ok(());
    }
    let slug = match slug {
        Some(slug) => match crate::git::normalize_name(&slug) {
            Ok(slug) => slug,
            Err(error) => {
                handles
                    .flow
                    .platform
                    .reply_text(message_id, &format!("⚠️ /worktree 失败：{error}"))
                    .await?;
                return Ok(());
            }
        },
        None => intent.slug.clone(),
    };
    let (base, base_sha) = match base {
        Some(base) => match crate::git::resolve_base(&pending.directory, Some(&base)).await {
            Ok(base) => base,
            Err(error) => {
                handles
                    .flow
                    .platform
                    .reply_text(message_id, &format!("⚠️ /worktree 失败：{error}"))
                    .await?;
                return Ok(());
            }
        },
        None => (intent.base.clone(), intent.base_sha.clone()),
    };
    let root = match crate::git::worktree_root(&pending.directory, handles.config.worktree_root()).await {
        Ok(root) => root,
        Err(error) => {
            handles
                .flow
                .platform
                .reply_text(message_id, &format!("⚠️ /worktree 失败：{error}"))
                .await?;
            return Ok(());
        }
    };
    let target = crate::git::plain_path(root.join(&slug))
        .to_string_lossy()
        .to_string();
    let updated = crate::bridge::session::WorktreeIntent {
        base,
        base_sha,
        slug,
        target,
        path: None,
    };
    if !handles
        .flow
        .sessions
        .update_pending(thread_key, |pending| {
            if let Some(worktree) = pending.worktree.as_mut() {
                *worktree = updated.clone();
            }
        })
        .await?
    {
        handles
            .flow
            .platform
            .reply_text(message_id, "当前没有待建会话。")
            .await?;
        return Ok(());
    }
    crate::bridge::topic::refresh_pending_cover(&handles.topic_handles(), thread_key).await;
    let mut pending = pending;
    pending.worktree = Some(updated.clone());
    handles
        .flow
        .platform
        .reply_text(message_id, &status_text(&pending, &updated))
        .await?;
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
