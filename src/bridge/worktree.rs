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
`/topic --worktree [--name <标题>] [--branch <分支>] [--base <ref>]`
`/new --worktree [--name <标题>] [--branch <分支>] [--base <ref>]`
- 在 `[worktree] root`（默认 `.worktrees`，相对主检出）下创建检出。
- `--branch` 是完整分支名，可带仓库的 `<type>/`（如 `feat/fix-login`）；目录取最后一段（`fix-login`）。不带 `/` 时分支和目录同名；不固定任何前缀。
- 起点按 `[worktree] base` → `origin/HEAD` → `origin/main` → `origin/master` → `main` → `master` 解析；`--base` 覆盖，并在命令时固定成 SHA。
- `--name` 是会话标题（可中文）；`--branch` 省略时从标题推导 slug，推不出用日期默认值。
- 命令只记录意图，第一条消息才真正创建检出；放弃话题不会留下检出。
- 待建期间用 `/worktree` 查看，`/worktree --branch <分支>` 或 `--base <ref>` 修改。已有检出用 `/dir <路径>` 进入，或 `/run git worktree list` 查看。";

/// The two "there is no checkout intent here" answers, shared by `/worktree`
/// and its adjuster so the routes cannot drift.
fn no_pending_reply() -> &'static str {
    "当前没有待建会话。\n- 新建带 worktree 的会话：`/topic --worktree`（新话题）或 `/new --worktree`（当前对话）。\n- 进入已有检出：`/dir <路径>`；查看已有检出：`/run git worktree list`。"
}

fn no_intent_reply(directory: &str) -> String {
    format!(
        "当前待建会话没有 worktree 意图（目录 `{directory}`）。\n- 要带 worktree：用 `/topic --worktree`（新话题）或 `/new --worktree`（当前对话）重新声明。\n- 只想换目录：用 `/dir <路径>`。"
    )
}

/// `/worktree`: report the pending checkout intent. Outside a pending it
/// answers the routes — the inventory card is retired, and discovery is
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
            Some(intent) => status_text(pending.title.as_deref(), &pending.directory, intent),
            None => no_intent_reply(&pending.directory),
        },
        None => no_pending_reply().to_string(),
    };
    handles.flow.platform.reply_text(message_id, &reply).await?;
    Ok(())
}

/// The status is the cover's facts in text form: target, branch, resolved
/// base, source, and the one-line route to change it.
fn status_text(
    title: Option<&str>,
    directory: &str,
    intent: &crate::bridge::session::WorktreeIntent,
) -> String {
    let title = title
        .map(|title| format!("\n标题：`{title}`"))
        .unwrap_or_default();
    let created = match intent.path.as_deref() {
        Some(path) if intent.checkout_exists() => {
            format!("\n检出已创建：`{path}`（改 branch/base 不再影响它）")
        }
        Some(path) => format!(
            "\n原检出已不存在：`{path}`；下一条消息会重建。若 `git worktree list` 仍登记旧路径或同名分支，重建会被拒绝——先 `/worktree --branch <新分支>` 换名即可"
        ),
        None => String::new(),
    };
    format!(
        "🌲 待建 worktree{title}\n目标：`{}`（若被占用会加后缀）\n分支：`{}` · 起点：`{} @ {}`\n源目录：`{}`{created}\n第一条消息在该目录创建会话。用 `/worktree --branch <分支>` 或 `--base <ref>` 修改。",
        intent.target,
        intent.branch_name(),
        intent.base,
        intent.short_sha(),
        directory
    )
}

/// `/worktree --branch <branch>` / `--base <ref>`: adjust the pending intent
/// before the first message creates it. Once the checkout exists (a failed
/// session create leaves the pending), the directory is fixed — the command
/// says so instead of pretending a rename would move it.
pub(crate) async fn adjust(
    handles: &CommandHandles,
    thread_key: &ThreadKey,
    message_id: &str,
    branch: Option<String>,
    base: Option<String>,
) -> crate::error::Result<()> {
    let (pending, epoch) = {
        let store = handles.flow.sessions.store.lock().await;
        (
            store.pending_for(thread_key).cloned(),
            store.pending_epoch_for(thread_key),
        )
    };
    let Some(pending) = pending else {
        handles
            .flow
            .platform
            .reply_text(message_id, no_pending_reply())
            .await?;
        return Ok(());
    };
    let Some(intent) = pending.worktree.clone() else {
        handles
            .flow
            .platform
            .reply_text(message_id, &no_intent_reply(&pending.directory))
            .await?;
        return Ok(());
    };
    let (branch, slug) = match branch {
        Some(branch) => match crate::git::normalize_branch(&branch) {
            Ok(pair) => pair,
            Err(error) => {
                handles
                    .flow
                    .platform
                    .reply_text(message_id, &format!("⚠️ /worktree 失败：{error}"))
                    .await?;
                return Ok(());
            }
        },
        None => (intent.branch_name(), intent.slug.clone()),
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
    let target = target_for(&root, &slug);
    let updated = crate::bridge::session::WorktreeIntent {
        base,
        base_sha,
        slug,
        branch,
        target,
        path: None,
    };
    let reply = match handles
        .flow
        .sessions
        .adjust_pending_worktree(thread_key, epoch, updated.clone())
        .await?
    {
        crate::bridge::session::WorktreeAdjust::Applied => {
            crate::bridge::topic::refresh_pending_cover(&handles.topic_handles(), thread_key).await;
            status_text(pending.title.as_deref(), &pending.directory, &updated)
        }
        crate::bridge::session::WorktreeAdjust::Creating => {
            "⏳ 这个会话正在创建中，等它完成后再改 worktree。".to_string()
        }
        crate::bridge::session::WorktreeAdjust::Created(path) => format!(
            "⚠️ 检出已创建：`{path}`；现在改 branch/base 不会移动它。第一条消息会在这个目录创建会话。"
        ),
        crate::bridge::session::WorktreeAdjust::Replaced => {
            "⚠️ 待建会话已被替换（`/new`、`/dir` 或 `/topic`），这次修改没有生效；请重新执行 `/worktree --branch` 或 `--base`。".to_string()
        }
        crate::bridge::session::WorktreeAdjust::NoPending => no_pending_reply().to_string(),
        crate::bridge::session::WorktreeAdjust::NoIntent => no_intent_reply(&pending.directory),
    };
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
/// non-bare git source), the integration base to a SHA, the branch/directory,
/// and the planned target. Nothing is checked out here; `create_worktree` runs
/// at materialisation, so an abandoned pending creates no checkout.
pub(crate) async fn intent(
    source: &str,
    root: &std::path::Path,
    name: Option<&str>,
    branch: Option<&str>,
    base: Option<&str>,
    configured_base: Option<&str>,
) -> Result<(crate::bridge::session::WorktreeIntent, std::path::PathBuf), crate::git::WorktreeError> {
    crate::git::ensure_creatable(source).await?;
    let (base, base_sha) = crate::git::resolve_base(source, base.or(configured_base)).await?;
    let (branch, slug) = match branch {
        Some(branch) => crate::git::normalize_branch(branch)?,
        None => {
            let slug = name
                .and_then(crate::git::derive_slug)
                .unwrap_or_else(|| default_slug(chrono::Utc::now()));
            (slug.clone(), slug)
        }
    };
    let root = crate::git::worktree_root(source, root).await?;
    // Refuse an uncreatable root at command time WITHOUT creating it: an
    // abandoned pending must leave nothing behind. Walk to the nearest
    // existing ancestor; materialisation does the actual `create_dir_all`.
    check_root_creatable(&root)?;
    let target = target_for(&root, &slug);
    Ok((
        crate::bridge::session::WorktreeIntent {
            base,
            base_sha,
            slug,
            branch,
            target,
            path: None,
        },
        root,
    ))
}

/// The planned checkout path. Both the declaration and the adjuster use this
/// one expression, so their target cannot drift.
fn target_for(root: &std::path::Path, slug: &str) -> String {
    crate::git::plain_path(root.join(slug))
        .to_string_lossy()
        .to_string()
}

/// Whether the root can plausibly be created, without creating it: walk up to
/// the nearest existing ancestor and require a directory. A permission failure
/// surfaces at materialisation (where the pending is kept); this check catches
/// the clear "a parent is a file" case at command time (ADR-0095).
fn check_root_creatable(root: &std::path::Path) -> Result<(), crate::git::WorktreeError> {
    let mut probe = root;
    loop {
        match std::fs::metadata(probe) {
            Ok(meta) if meta.is_dir() => return probe_writable(probe, root),
            Ok(_) => {
                return Err(crate::git::WorktreeError::new(format!(
                    "`{}` 已存在但不是目录",
                    probe.display()
                )));
            }
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound
                    || error.kind() == std::io::ErrorKind::NotADirectory =>
            {
                match probe.parent() {
                    Some(parent) if parent != probe => probe = parent,
                    _ => {
                        return Err(crate::git::WorktreeError::new(format!(
                            "找不到可用的父目录：`{}`",
                            root.display()
                        )));
                    }
                }
            }
            Err(error) => {
                return Err(crate::git::WorktreeError::new(format!(
                    "无法检查目录 `{}`：{error}",
                    probe.display()
                )));
            }
        }
    }
}

/// Prove the nearest existing directory is writable by creating and removing
/// one probe directory. A stat cannot answer "creatable" on a read-only
/// filesystem, and creating the real root would leave it behind when the
/// pending is abandoned (ADR-0095).
fn probe_writable(
    directory: &std::path::Path,
    root: &std::path::Path,
) -> Result<(), crate::git::WorktreeError> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let probe = directory.join(format!(".cola-worktree-probe-{}-{stamp}", std::process::id()));
    match std::fs::create_dir(&probe) {
        Ok(()) => {
            let _ = std::fs::remove_dir(&probe);
            Ok(())
        }
        Err(error) => Err(crate::git::WorktreeError::new(format!(
            "无法在 `{}` 创建目录（`{}` 不可写？）：{error}",
            root.display(),
            directory.display()
        ))),
    }
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
        intent.branch_name(),
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
