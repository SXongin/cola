//! The command card forms (spec #298, ticket D): the state access and delivery
//! around the command cards, joining the structured-input → card-JSON builders
//! the card layer already owns.
//!
//! The JSON builders keep their card families and their contract — structured
//! inputs in, card JSON out: [`super::session::build_switch_card`],
//! [`super::session::build_dir_card`] and [`super::session::build_child_card`],
//! [`super::picker::build_agent_card`],
//! [`super::picker::build_model_surface_cards`],
//! [`super::picker::build_think_card`],
//! [`super::picker::build_autoaccept_card`], and
//! [`super::help::build_help_card`]. This module is the form layer over them:
//! it fetches the state each builder takes as its input, hands it over, and
//! sends the result — the fetch → build → send path for the cards the slash
//! commands pop (`/switch`, `/dir`, `/sub`, `/agent`, `/model`, `/think`,
//! `/autoaccept`, `/help`). `bridge::command` keeps the parser, the dispatch
//! match and the text-command behavior.
//!
//! The fetch-and-shape functions ([`switch_card_data`], [`dir_card_data`],
//! [`child_card_data`], [`agent_card`], [`think_card`], [`model_surface`])
//! are shared with the coordinator's card-ack refresh, which rebuilds a clicked
//! card from the same one source of truth.

use serde_json::Value;

use crate::bridge::command::{help_text, matches_keyword};
use crate::bridge::display::feishu_side_label;
use crate::bridge::handles::CommandHandles;
use crate::config::ThreadKey;
use crate::error::Result;

use super::session::BrowseView;
use super::session::SwitchScope;

/// Fetch + shape the data the `/switch` card renders: the session list
/// (children and archived excluded, filtered by `keyword`, sorted by last
/// activity) plus the thread's active + mapped session ids. The list is scoped
/// by `scope` (ADR-0022): `Directory` filters to the current directory (the
/// Pending Session's when one exists, else the active session's — ADR-0041),
/// falling back to the whole store when the thread has neither; `All` shows
/// everything. Returns the effective scope (the fallback may downgrade
/// `Directory` to `All`) and the current directory, so the card can render the
/// right header and toggle. Shared by the text send path (`send_switch_card`)
/// and the card ack refresh (`App::build_switch_card_for`) so both render from
/// one source of truth.
pub(crate) async fn switch_card_data(
    handles: &CommandHandles,
    thread_key: &ThreadKey,
    keyword: &str,
    scope: SwitchScope,
) -> (
    Vec<crate::backend::types::SessionListInfo>,
    Option<String>,
    Vec<String>,
    SwitchScope,
    Option<String>,
) {
    let sessions = handles
        .flow
        .sessions
        .cached_session_list(&handles.flow.backend)
        .await
        .unwrap_or_default();
    // Pending-first (ADR-0041): a declared Pending Session defines the current
    // directory even though `get_active` is `None` for the thread.
    let current_dir = handles
        .flow
        .sessions
        .store
        .lock()
        .await
        .current_directory(thread_key);
    // Directory scope only holds when there IS a current directory; a fresh
    // conversation (no active session) falls back to the whole store.
    let scope = if scope == SwitchScope::Directory && current_dir.is_some() {
        SwitchScope::Directory
    } else {
        SwitchScope::All
    };
    let lower = keyword.to_lowercase();
    let mut shown: Vec<crate::backend::types::SessionListInfo> = sessions
        .into_iter()
        .filter(|s| {
            !s.is_child()
                && !s.is_archived()
                && if lower.is_empty() {
                    true
                } else {
                    matches_keyword(s, &lower)
                }
                && (scope != SwitchScope::Directory || current_dir.as_deref() == Some(s.directory.as_str()))
        })
        .collect();
    shown.sort_by(|a, b| {
        let ub = b.time.as_ref().map(|t| t.updated).unwrap_or(0);
        let ua = a.time.as_ref().map(|t| t.updated).unwrap_or(0);
        ub.cmp(&ua)
    });
    let (active_id, mapped_ids) = {
        let store = handles.flow.sessions.store.lock().await;
        let active = store.get_active(thread_key).map(|e| e.session_id.clone());
        let mapped: Vec<String> = store
            .list_thread(thread_key)
            .into_iter()
            .map(|e| e.session_id.clone())
            .collect();
        (active, mapped)
    };
    (shown, active_id, mapped_ids, scope, current_dir)
}

/// Build and send the interactive `/switch` session card (ADR-0012, issue 04,
/// ADR-0022). Renders the filtered session list (via `switch_card_data`) and
/// replies with the card, starting on page 1 (ADR-0052).
pub(crate) async fn send_switch_card(
    handles: &CommandHandles,
    thread_key: &ThreadKey,
    keyword: &str,
    scope: SwitchScope,
    message_id: &str,
) -> Result<()> {
    let (shown, active_id, mapped_ids, scope, current_dir) =
        switch_card_data(handles, thread_key, keyword, scope).await;
    let card = super::session::build_switch_card(
        thread_key,
        &shown,
        keyword,
        scope,
        1,
        current_dir.as_deref(),
        active_id.as_deref(),
        &mapped_ids,
    );
    handles.flow.platform.reply_card(message_id, &card).await?;
    Ok(())
}

/// Fetch + shape the data the `/sub` child-session card renders (spec #344):
/// the Active Session's DIRECT children (`parentID` equal to its id), keyword-
/// filtered and sorted by last activity newest-first, plus the live run state
/// of each row on the requested page — one `session_status` read per rendered
/// row, bounded by the page size. A conversation with no Active Session (a
/// fresh chat, or a Pending Session superseding the mapping — ADR-0041) has no
/// children by construction: the list comes back empty and no status is read.
/// Archived children are excluded, like every other session surface. The
/// status map's `None` (a failed or unrecognised read) omits the row's label
/// rather than guessing (ADR-0028).
pub(crate) async fn child_card_data(
    handles: &CommandHandles,
    thread_key: &ThreadKey,
    keyword: &str,
    page: usize,
) -> (
    Vec<crate::backend::types::SessionListInfo>,
    std::collections::HashMap<String, Option<crate::backend::types::SessionStatus>>,
) {
    let Some(active_id) = handles.flow.sessions.get_session_id(thread_key).await else {
        return (Vec::new(), std::collections::HashMap::new());
    };
    let sessions = handles
        .flow
        .sessions
        .cached_session_list(&handles.flow.backend)
        .await
        .unwrap_or_default();
    let lower = keyword.to_lowercase();
    let mut children: Vec<crate::backend::types::SessionListInfo> = sessions
        .into_iter()
        .filter(|s| {
            s.is_child_of(active_id.as_str())
                && !s.is_archived()
                && (lower.is_empty() || matches_keyword(s, &lower))
        })
        .collect();
    children.sort_by(|a, b| {
        let ub = b.time.as_ref().map(|t| t.updated).unwrap_or(0);
        let ua = a.time.as_ref().map(|t| t.updated).unwrap_or(0);
        ub.cmp(&ua)
    });

    // One status read per row actually rendered: the window is clamped here the
    // same way the builder clamps it, so pages do not read their siblings' rows.
    let window = super::session::page_window(children.len(), page);
    let mut statuses = std::collections::HashMap::new();
    for child in &children[window.start..window.end] {
        let status = handles
            .flow
            .backend
            .session_status(&child.id, Some(&child.directory))
            .await
            .ok()
            .flatten();
        statuses.insert(child.id.clone(), status);
    }
    (children, statuses)
}

/// Build and send the read-only `/sub` child-session card (spec #344): the
/// Active Session's direct children, starting on page 1. A conversation with
/// no Active Session opens the card's plain empty state.
pub(crate) async fn send_child_card(
    handles: &CommandHandles,
    thread_key: &ThreadKey,
    keyword: &str,
    message_id: &str,
) -> Result<()> {
    let (children, statuses) = child_card_data(handles, thread_key, keyword, 1).await;
    let card = super::session::build_child_card(
        thread_key,
        &children,
        &statuses,
        keyword,
        1,
        chrono::Utc::now().timestamp_millis(),
    );
    handles.flow.platform.reply_card(message_id, &card).await?;
    Ok(())
}

/// Fetch + shape the data the `/dir` Recent Directories card renders: distinct
/// directories of recently-active sessions (children and archived excluded,
/// deduped by directory, sorted by last activity), unioned with the
/// directories cola has mapped, plus the thread's current directory. The
/// session list alone loses a directory as soon as its last session is
/// deleted or archived (OpenChamber's retention, `opencode session delete`),
/// while the SessionStore is cola's own file and keeps its mappings. A
/// non-empty `keyword` then narrows the union to paths matching it (ADR-0051),
/// current directory included. Shared by the text send path (`send_dir_card`)
/// and the card ack refresh (`App::build_dir_card_for`) so both render from
/// one source of truth.
pub(crate) async fn dir_card_data(
    handles: &CommandHandles,
    thread_key: &ThreadKey,
    keyword: &str,
) -> (Vec<String>, Option<String>) {
    let sessions = handles
        .flow
        .sessions
        .cached_session_list(&handles.flow.backend)
        .await
        .unwrap_or_default();
    // Directory -> latest activity. A directory's freshness is its most
    // recently active session's `time.updated`.
    let mut by_dir: Vec<(String, i64)> = Vec::new();
    for s in sessions {
        if s.is_child() || s.is_archived() {
            continue;
        }
        let updated = s.time.as_ref().map(|t| t.updated).unwrap_or(0);
        if let Some(entry) = by_dir.iter_mut().find(|(d, _)| *d == s.directory) {
            entry.1 = entry.1.max(updated);
        } else {
            by_dir.push((s.directory, updated));
        }
    }
    by_dir.sort_by_key(|(_, updated)| std::cmp::Reverse(*updated));
    let mut dirs: Vec<String> = by_dir.into_iter().map(|(d, _)| d).collect();
    // Pending-first (ADR-0041): `get_active` is `None` while a pending exists.
    let (mapped_dirs, current_dir) = {
        let store = handles.flow.sessions.store.lock().await;
        (store.directories(), store.current_directory(thread_key))
    };
    // The store lists directories most recently mapped first — the sensible
    // tail position for directories the session list no longer carries (their
    // sessions were deleted or archived).
    for dir in mapped_dirs {
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    // A Pending Session's directory has no server session yet and no mapping
    // entry (ADR-0041), so neither source above carries it; the card still has
    // to render it as `当前` rather than fall back to the empty-state hint.
    if let Some(current) = current_dir.as_ref()
        && !dirs.contains(current)
    {
        dirs.insert(0, current.clone());
    }
    // ADR-0051: the card's keyword narrows the union — the current directory
    // included, so a keyword that does not match it drops its row.
    if !keyword.is_empty() {
        let lower = keyword.to_lowercase();
        dirs.retain(|dir| matches_directory(dir, &lower));
    }
    (dirs, current_dir)
}

/// Case-insensitive token-AND match on a directory path (ADR-0051): the
/// `/switch` search box's rule applied to the path only — `lower_keyword` is
/// already lowercased, split on whitespace, and every token must appear as a
/// substring.
fn matches_directory(dir: &str, lower_keyword: &str) -> bool {
    crate::bridge::command::token_and_match(&dir.to_lowercase(), lower_keyword)
}

/// Build and send the interactive `/dir` Recent Directories card. Renders the
/// deduped directory list (via `dir_card_data`) and replies with the card,
/// starting on page 1 (ADR-0052).
pub(crate) async fn send_dir_card(
    handles: &CommandHandles,
    thread_key: &ThreadKey,
    message_id: &str,
) -> Result<()> {
    let (dirs, current_dir) = dir_card_data(handles, thread_key, "").await;
    let card = super::session::build_dir_card(thread_key, &dirs, current_dir.as_deref(), "", 1);
    handles.flow.platform.reply_card(message_id, &card).await?;
    Ok(())
}

/// The `/worktree` card's data (ADR-0088): the project's checkouts from
/// `git worktree list`, each with its dirty state (bounded per checkout), the
/// number of non-child, non-archived sessions rooted in it, and whether the
/// conversation is already there. `Err` is the readable line for a non-git
/// project (or a read past the deadline) — answered as text, never as an empty
/// card.
///
/// The whole read is bounded because a card callback runs on a deadline
/// (`CODING_STANDARDS.md`): `git worktree list` is a process launch too, and a
/// stalled filesystem must not outlive the ack budget.
pub(crate) async fn worktree_card_data(
    handles: &CommandHandles,
    thread_key: &ThreadKey,
) -> std::result::Result<(Vec<super::session::WorktreeRow>, String), String> {
    // ONE budget for the whole read — the inventory and the dirty/session
    // rows — so the card's worst case is the 5 s the ADR promises, not a sum
    // of per-phase bounds.
    let deadline = tokio::time::Instant::now() + crate::git::DIRTY_BUDGET;
    let repo = handles.current_project_directory(thread_key).await;
    let entries = match tokio::time::timeout_at(deadline, crate::git::list_worktrees(&repo)).await {
        Ok(Ok(entries)) => entries,
        Ok(Err(error)) => return Err(error.to_string()),
        Err(_) => return Err("读取 worktree 超时，请稍后再试".to_string()),
    };
    let rows = worktree_rows(handles, thread_key, entries, deadline).await;
    Ok((rows, repo))
}

/// Build the card rows for an already-read inventory (ADR-0088): dirty state
/// and the cached session list under `deadline` (`?` / zero past it), and the
/// current marker.
///
/// Separate from [`worktree_card_data`] so a card callback — which runs on a
/// deadline — can reuse the inventory it just read for validation and give the
/// remaining work a budget that fits the ack.
pub(crate) async fn worktree_rows(
    handles: &CommandHandles,
    thread_key: &ThreadKey,
    entries: Vec<crate::git::WorktreeEntry>,
    deadline: tokio::time::Instant,
) -> Vec<super::session::WorktreeRow> {
    let paths: Vec<String> = entries.iter().map(|entry| entry.path.clone()).collect();
    let dirty = crate::git::worktree_dirty_until(&paths, deadline).await;
    // The conversation's current directory is pending-first, then the active
    // session's; with neither, `/new` falls back to the default directory, and
    // the card marks that same checkout (ADR-0088).
    let current = match handles.flow.sessions.current_directory(thread_key).await {
        Some(dir) => Some(dir),
        None => Some(handles.current_project_directory(thread_key).await),
    };
    let sessions = tokio::time::timeout_at(
        deadline,
        handles.flow.sessions.cached_session_list(&handles.flow.backend),
    )
    .await
    .ok()
    .and_then(std::result::Result::ok)
    .unwrap_or_default();
    entries
        .into_iter()
        .zip(dirty)
        .map(|(entry, dirty)| {
            let name = std::path::Path::new(&entry.path)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| entry.path.clone());
            let count = sessions
                .iter()
                .filter(|session| {
                    !session.is_child()
                        && !session.is_archived()
                        && crate::bridge::command::same_dir(&session.directory, &entry.path)
                })
                .count();
            let current = current
                .as_deref()
                .is_some_and(|dir| crate::bridge::command::same_dir(dir, &entry.path));
            super::session::WorktreeRow {
                path: entry.path,
                name,
                branch: entry.branch,
                dirty,
                main: entry.main,
                sessions: count,
                current,
            }
        })
        .collect()
}

/// Send the `/worktree` card, or — for a directory that is not a git
/// repository — the reason as text.
pub(crate) async fn send_worktree_card(
    handles: &CommandHandles,
    thread_key: &ThreadKey,
    message_id: &str,
) -> Result<()> {
    match worktree_card_data(handles, thread_key).await {
        Ok((rows, repo)) => {
            let card = super::session::build_worktree_card(&rows, &repo);
            handles.flow.platform.reply_card(message_id, &card).await?;
        }
        Err(error) => {
            handles
                .flow
                .platform
                .reply_text(message_id, &format!("⚠️ /worktree：{error}"))
                .await?;
        }
    }
    Ok(())
}

/// The bare `/cat` usage line: `/cat` with no path answers this instead of
/// being forwarded as a prompt (ADR-0083 §2).
pub(crate) const CAT_USAGE: &str = "\
用法：`/cat <路径>` —— 在卡片里查看宿主机上的一个文件（目录则打开该目录的浏览卡，不会报错）。
- 相对路径按**当前会话目录**解析；会话还没创建时用当前项目目录，都没有则用默认目录。
- `~` 展开为 home，绝对路径（包括会话目录之外的）原样使用。
- 只是隐私提示，不是沙箱；谁能操作仍由 Access List 决定，而群聊里卡片人人可读。
例：`/cat src/main.rs`、`/cat ~/notes.md`、`/cat /etc/hosts`";

/// Open `/cat <path>` (ADR-0083 §2, §3): the File Viewer card for a file, the
/// Directory Browser for a directory, the usage line for a bare `/cat`, and —
/// for a path that cannot be shown — that path's own honest answer, never an
/// empty card. The bytes are read locally (`bridge::viewer`), so the command
/// is generation- and backend-neutral, and the answer is a plain command card:
/// it joins no Card Chain, is never written to the Chain Record and is never
/// reaped.
pub(crate) async fn send_cat_card(
    handles: &CommandHandles,
    thread_key: &ThreadKey,
    message_id: &str,
    arg: Option<&str>,
) -> Result<()> {
    let platform = &handles.flow.platform;
    let Some(arg) = arg.map(str::trim).filter(|a| !a.is_empty()) else {
        platform.reply_text(message_id, CAT_USAGE).await?;
        return Ok(());
    };
    // The Path Scope (ADR-0083 §4): `current_project_directory` is pending-first
    // — the Pending Session's directory wins, else the active session's, else
    // the default — so a conversation with no session yet still resolves
    // against its current project.
    let session_dir = handles.current_project_directory(thread_key).await;
    let timeout = handles.flow.cards.viewer_transfer_timeout_ms();
    let read = crate::bridge::handler::read_cat_path_bounded(
        arg.to_string(),
        session_dir.clone(),
        timeout,
        handles.flow.cards.viewer_fs_read_slots.clone(),
    )
    .await;
    match read {
        Ok(crate::bridge::handler::CatPathRead::Directory(dir)) => {
            // `/cat <directory>` answers the Directory Browser there rather
            // than erroring: `/cat` always means "show me this path".
            let dir = dir.to_string_lossy().to_string();
            let card = crate::bridge::handler::build_dir_browse_card_for(
                &handles.flow.sessions,
                thread_key,
                BrowseView {
                    dir: &dir,
                    show_hidden: false,
                    keyword: "",
                    page: 1,
                },
            )
            .await;
            platform.reply_card(message_id, &card).await?;
        }
        Ok(crate::bridge::handler::CatPathRead::File { path, mut classified }) => {
            // Classify by CONTENT (ADR-0083 §6) and deliver the whole-bytes lane
            // the file takes (ADR-0083 §5): an image embeds, a binary (or a text
            // past the preview cap) goes out as one File Message under this
            // command's own message. The bounded read reports an unreadable
            // file's own cause instead.
            let name = file_display_name(&path);
            // The byte lanes first (#739), then #741's structural-overflow
            // lane for a text preview the card cannot hold — one shared
            // helper, so `/cat` and a file-row/tool-open click cannot
            // disagree about where a preview went.
            let ext = path.extension().and_then(|e| e.to_str());
            let (lane, overflowed) = crate::bridge::viewer::open_viewer_lanes(
                platform,
                &handles.flow.cards,
                message_id,
                &name,
                ext,
                &mut classified,
                crate::bridge::viewer::ViewerLaneOptions {
                    timeout_ms: timeout,
                    existing_rich_message_id: None,
                    existing_sent_file_identity: None,
                    existing_ambiguous_file_identity: None,
                    existing_embedded_image_key: None,
                },
            )
            .await;
            let card = build_viewer_for_classified(
                thread_key,
                &path,
                &classified,
                lane,
                None,
                ViewerBuildOptions {
                    page: 1,
                    overflowed,
                    session_dir: Some(&session_dir),
                    // A `/cat` open leads with the default view (#781).
                    view: None,
                    notice: None,
                },
            );
            platform.reply_card(message_id, &card).await?;
        }
        Ok(crate::bridge::handler::CatPathRead::Failure(line)) => {
            platform.reply_text(message_id, &line).await?;
        }
        Err(
            crate::bridge::handler::ViewerFsReadError::Busy(line)
            | crate::bridge::handler::ViewerFsReadError::Timeout(line),
        ) => {
            platform.reply_text(message_id, &line).await?;
        }
        Err(crate::bridge::handler::ViewerFsReadError::Worker(error)) => {
            platform
                .reply_text(message_id, &format!("⚠️ 无法读取文件：{error}"))
                .await?;
        }
    }
    Ok(())
}

/// The file name a viewer names and a File Message uploads under: the path's
/// last component, or `file` when the path has none.
pub(crate) fn file_display_name(path: &std::path::Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".to_string())
}

/// Build the **File Viewer** card for a file already classified by content
/// (ADR-0083 §3, §5). Shared by `/cat` and the 「发为文件」 transfer — which
/// PATCHes the card it was clicked on with the outcome — so both name the same
/// file the same way and render the same lane. `page` is the 1-based page of
/// the line-windowed text preview (#740). `overflowed` is #741's LANDED lane:
/// the preview went out as its own Rich Text (or File) Message and landed, so
/// the card carries no preview, no truncation note and no 「发为文件」 —
/// naming, the tracking line and the directory buttons. `false` whenever the
/// lane did not land (a fitting preview, or a failed send that keeps its
/// preview and its rescue button).
pub(crate) struct ViewerBuildOptions<'a> {
    pub(crate) page: usize,
    pub(crate) overflowed: bool,
    pub(crate) session_dir: Option<&'a str>,
    /// The markdown view to show (#781), or `None` for the default (rendered
    /// when the render fits the card, source otherwise).
    pub(crate) view: Option<super::viewer::ViewerView>,
    /// A one-line notice when a rebuild found the file changed under it (a
    /// pager flip whose payload's lane no longer matches the fresh bytes), or
    /// `None`. Rendered where the tracking line would be.
    pub(crate) notice: Option<&'a str>,
}

pub(crate) fn build_viewer_for_classified(
    thread_key: &ThreadKey,
    path: &std::path::Path,
    classified: &crate::bridge::viewer::Classified,
    lane: Option<super::viewer::ViewerFileLane>,
    return_to: Option<BrowseView<'_>>,
    options: ViewerBuildOptions<'_>,
) -> Value {
    use super::viewer::{ViewerCard, ViewerKind, build_viewer_card, kind_for_extension};
    use crate::bridge::viewer::ClassifiedKind;

    let ext = path.extension().and_then(|e| e.to_str());
    let ViewerBuildOptions {
        page,
        overflowed,
        session_dir,
        view,
        notice,
    } = options;
    let kind = match classified.kind {
        ClassifiedKind::Image => ViewerKind::Image,
        ClassifiedKind::File => ViewerKind::Binary,
        ClassifiedKind::TextTooLarge => ViewerKind::TextTooLarge,
        ClassifiedKind::Text => kind_for_extension(ext),
    };
    let shown = path.to_string_lossy().to_string();
    let file_name = file_display_name(path);
    let content_identity = lane.as_ref().and_then(|lane| lane.content_identity).or_else(|| {
        classified
            .bytes
            .as_deref()
            .or_else(|| classified.text.as_deref().map(str::as_bytes))
            .map(|bytes| crate::bridge::handles::content_identity_hash(&classified.mime, &file_name, bytes))
    });
    build_viewer_card(
        thread_key,
        ViewerCard {
            path: &shown,
            session_dir,
            size: classified.size,
            kind,
            ext,
            text: classified.text.as_deref().unwrap_or(""),
            content_identity,
            truncated: false,
            page,
            view,
            return_to,
            lane,
            overflowed,
            notice,
        },
    )
}

/// The first agent in `GET /agent` order that the server would actually run as
/// its default: primary (not subagent) and visible (not hidden). The server's
/// own agent service sorts the configured default (or `build`) first, so the
/// first primary visible agent in that order IS the server default — no
/// `/config` round trip needed. A thin alias over
/// [`AgentInfo::default_agent`](crate::backend::types::AgentInfo::default_agent)
/// so the card and V2's `/agent --reset` switch resolve one rule.
fn server_default_agent(agents: &[crate::backend::types::AgentInfo]) -> Option<String> {
    crate::backend::types::AgentInfo::default_agent(agents)
}

/// Resolve what the `/agent` card should show for a thread's target session:
/// the per-session override if set, else the server's default agent, plus the
/// agent list. The target is the Pending Session when one exists, else the
/// active SessionEntry (ADR-0041). Shared by the text send path and the
/// card-ack refresh so both render the current agent from one source of truth.
pub(crate) async fn agent_card(
    handles: &CommandHandles,
    thread_key: &ThreadKey,
) -> (Option<Value>, Option<String>) {
    let Some(settings) = handles.flow.sessions.session_settings(thread_key).await else {
        return (
            None,
            Some(format!(
                "{}还没有会话，先用 `/new` 或 `/dir` 创建。",
                feishu_side_label(thread_key)
            )),
        );
    };
    let agents = handles.flow.backend.list_agents().await;
    let default = server_default_agent(&agents);
    let card =
        super::picker::build_agent_card(thread_key, &agents, settings.agent.as_deref(), default.as_deref());
    (Some(card), None)
}

/// Send the `/agent` picker card, or a text explanation when there is no
/// active session.
pub(crate) async fn send_agent_card(
    handles: &CommandHandles,
    thread_key: &ThreadKey,
    message_id: &str,
) -> Result<()> {
    let (card, error) = agent_card(handles, thread_key).await;
    if let Some(c) = card {
        handles.flow.platform.reply_card(message_id, &c).await?;
    } else {
        handles
            .flow
            .platform
            .reply_text(message_id, &error.unwrap_or_default())
            .await?;
    }
    Ok(())
}

/// Send the `/model` card surface's root page. The page is whatever the
/// client declared (`Backend::model_surface`) — OpenCode's provider list,
/// Codex's flat catalog — rather than a shape the bridge assumes; the current
/// model line comes from the same surface.
pub(crate) async fn send_model_card(
    handles: &CommandHandles,
    thread_key: &ThreadKey,
    message_id: &str,
) -> Result<()> {
    // A client can be lazy (Codex attaches or starts its daemon on demand), and
    // the catalog/config the card needs are unreachable before that pass: the
    // card would render "no models" on a brand-new chat. `/model` is a demand
    // path like a prompt, so run the client-owned ensure pass first; a failure
    // is best-effort and the surface then carries the unattached note.
    if let Err(error) = crate::bridge::pollers::ensure_server(&handles.flow).await {
        tracing::debug!("the model card's ensure-attached pass failed (best-effort): {error}");
    }
    let surface = model_surface(handles, thread_key, None, None, None).await;
    let cards = super::picker::build_model_surface_cards(thread_key, &surface, false);
    for card in cards {
        handles.flow.platform.reply_card(message_id, &card).await?;
    }
    Ok(())
}

/// The current-selection ladder may use at most 1/N of a callback's remaining
/// budget; the backend read gets the rest.
const LADDER_SHARE_DIVISOR: u32 = 2;

/// Read one page of the client-declared `/model` surface. `scope`/`selected`
/// name the drill-down (OpenCode: the `provider` scope and the provider id the
/// user tapped); both are `None` for the root page. `deadline` is the card
/// callback's absolute deadline (`now + CARD_ACK_BUDGET`) when this read answers
/// a click, and `None` for a command's page.
///
/// `current` is resolved **only for the root page**. The running selection is
/// a root-page fact for every client we have: OpenCode never rendered it on its
/// model page (its drilled surface omits it) and Codex has no drill-down. It is
/// also a budget rule, not just an unused value: resolving it walks the
/// effective-model ladder, whose server-recorded rung can take
/// `SESSION_INFO_TIMEOUT` (3s) — longer than the card-ack budget (2s). A
/// callback therefore gives the ladder at most **half** of the budget still
/// left when the page read starts, then hands the backend only the remainder;
/// if the ladder misses its slice the page renders without the current line
/// instead of blowing the ack. A command's root page (`deadline = None`) keeps
/// the unlimited ladder.
pub(crate) async fn model_surface(
    handles: &CommandHandles,
    thread_key: &ThreadKey,
    scope: Option<&str>,
    selected: Option<&str>,
    deadline: Option<tokio::time::Instant>,
) -> crate::backend::types::ModelSurface {
    // The store read is part of the callback's work: charge it to the same
    // deadline, so a wedged store cannot leave the ladder and backend a fresh
    // budget. `None` is a command's page, with no ack to charge.
    let settings = match deadline {
        Some(deadline) => {
            match tokio::time::timeout_at(deadline, handles.flow.sessions.session_settings(thread_key)).await
            {
                Ok(settings) => settings,
                Err(_) => {
                    tracing::warn!(
                        "model card's session-settings read missed the callback budget; \
                         rendering without the thread's context"
                    );
                    None
                }
            }
        }
        None => handles.flow.sessions.session_settings(thread_key).await,
    };
    let current = match &settings {
        Some(settings) if scope.is_none() => {
            let resolve = handles
                .flow
                .sessions
                .effective_selection(&handles.flow.backend, settings);
            let resolved = match deadline {
                Some(deadline) => {
                    let now = tokio::time::Instant::now();
                    // The ladder may use at most half of what is left; the
                    // backend read gets the other half.
                    let ladder_deadline =
                        now + deadline.saturating_duration_since(now) / LADDER_SHARE_DIVISOR;
                    match tokio::time::timeout_at(ladder_deadline, resolve).await {
                        Ok(current) => current,
                        Err(_) => {
                            tracing::warn!(
                                "model card's current-selection ladder missed its callback slice; \
                                 rendering the page without the current line"
                            );
                            None
                        }
                    }
                }
                None => resolve.await,
            };
            resolved.map(|model| {
                let variant = model
                    .variant
                    .as_deref()
                    .map(|v| format!("@{v}"))
                    .unwrap_or_default();
                crate::backend::types::ModelSelection {
                    label: format!("{}/{}{}", model.provider_id, model.id, variant),
                    reference: crate::backend::parsing::join_model(&model.provider_id, &model.id),
                }
            })
        }
        // A drilled page (`scope.is_some()`) or a thread with no target has no
        // current line to resolve.
        _ => None,
    };
    // A pair a confirmation recorded but the thread has not run yet (#908):
    // shown separately from the current line, which is the thread fact. Only a
    // client with a thread-level provider defers a pair this way.
    let pending = settings
        .as_ref()
        .filter(|_| handles.flow.backend.has_thread_provider())
        .and_then(|settings| settings.model.as_deref());
    let session_id = settings.as_ref().and_then(|s| s.session_id.as_deref());
    let directory = settings
        .as_ref()
        .map(|s| s.directory.as_str())
        .filter(|directory| !directory.is_empty());
    // Only what is left after the ladder: a slow ladder cannot also give the
    // backend the whole budget. A passed deadline yields `Some(0)`, which every
    // adapter treats as "no time".
    let ack_budget = deadline.map(|deadline| deadline.saturating_duration_since(tokio::time::Instant::now()));
    handles
        .flow
        .backend
        .model_surface(crate::backend::types::ModelSurfaceQuery {
            session_id,
            directory,
            current,
            pending,
            scope,
            selected,
            // A card page, not `/think`'s variant lookup: the full surface.
            catalog_only: false,
            ack_budget,
        })
        .await
}

/// Resolve what the `/think` card should show for a thread's target session:
/// the effective model (settings override → configured default →
/// server-recorded) and its declared variants. The target is the Pending
/// Session when one exists, else the active SessionEntry (ADR-0041). Returns
/// `(card, error_text)` with exactly one set — an error text when the
/// conversation has no target, no model can be resolved, or the model declares
/// no variants (the caller then replies text instead of a card). Shared by the
/// text send path and the card-ack refresh so both render the current selection
/// from one source of truth.
pub(crate) async fn think_card(
    handles: &CommandHandles,
    thread_key: &ThreadKey,
) -> (Option<Value>, Option<String>) {
    let Some(settings) = handles.flow.sessions.session_settings(thread_key).await else {
        return (
            None,
            Some(format!(
                "{}还没有会话，先用 `/new` 或 `/dir` 创建。",
                feishu_side_label(thread_key)
            )),
        );
    };
    let Some(model) = handles
        .flow
        .sessions
        .effective_selection(&handles.flow.backend, &settings)
        .await
    else {
        return (
            None,
            Some("无法确定当前模型，请先用 `/model` 选择模型。".to_string()),
        );
    };
    let variants = handles
        .flow
        .sessions
        .model_variants(&handles.flow.backend, &model.provider_id, &model.id)
        .await
        .unwrap_or_default();
    if variants.is_empty() {
        return (
            None,
            Some(format!(
                "当前模型 `{}/{}` 没有思考等级可选。",
                model.provider_id, model.id
            )),
        );
    }
    let card = super::picker::build_think_card(
        thread_key,
        &model.provider_id,
        &model.id,
        model.variant.as_deref(),
        &variants,
    );
    (Some(card), None)
}

/// Send the `/think` variant-picker card, or a text explanation when no card
/// applies (no session / no resolvable model / the model declares no variants).
pub(crate) async fn send_think_card(
    handles: &CommandHandles,
    thread_key: &ThreadKey,
    message_id: &str,
) -> Result<()> {
    let (card, error) = think_card(handles, thread_key).await;
    if let Some(c) = card {
        handles.flow.platform.reply_card(message_id, &c).await?;
    } else {
        handles
            .flow
            .platform
            .reply_text(message_id, &error.unwrap_or_default())
            .await?;
    }
    Ok(())
}

/// Send the `/autoaccept` toggle card (ADR-0012, issue 05). The shown state is
/// the settings target's (Pending first, else active — ADR-0041).
pub(crate) async fn send_autoaccept_card(
    handles: &CommandHandles,
    thread_key: &ThreadKey,
    message_id: &str,
) -> Result<()> {
    let current_on = handles
        .flow
        .sessions
        .session_settings(thread_key)
        .await
        .map(|s| s.auto_accept)
        .unwrap_or(false);
    let card = super::picker::build_autoaccept_card(thread_key, current_on);
    handles.flow.platform.reply_card(message_id, &card).await?;
    Ok(())
}

/// Send the `/help` reference card (a pure command manual, no buttons; detail
/// stays text via `/help <command>`). If the card fails to send (e.g. Feishu
/// rejects the schema), fall back to the plain-text `help_text()` so the user
/// always gets something instead of a silent dead `/help`.
pub(crate) async fn send_help_card(handles: &CommandHandles, message_id: &str) -> Result<()> {
    let card = super::help::build_help_card();
    match handles.flow.platform.reply_card(message_id, &card).await {
        Ok(_) => Ok(()),
        Err(e) => {
            tracing::warn!("help card failed ({}), falling back to text", e);
            handles.flow.platform.reply_text(message_id, &help_text()).await?;
            Ok(())
        }
    }
}

/// Send the `/skill` picker card (spec #652, ticket #656; paged + searchable per
/// #664): every registered skill, one row each — the source of truth for a user
/// who does not know an id. The caller passes the list it already read for the
/// dispatch, so a dispatch reads the skills exactly ONCE. `error`, when set,
/// leads the card so an unknown-id dispatch shows the same list behind an error
/// line. `chat_type` rides the buttons so a tap re-enters the message pipeline
/// in the Chat/Topic kind the picker was sent in. The read behind `skills` is
/// generation-neutral and unfiltered, so a hidden or description-less skill is
/// listed too; an empty list opens the no-skills state. A catalog longer than a
/// page renders a search box and a pager instead of several cards.
///
/// A rejected card (Feishu refuses the schema/size) must not leave the command
/// dead, so — like [`send_help_card`]'s `help_text()` fallback — the failure
/// degrades to a plain-text listing of the same skills.
pub(crate) async fn send_skill_card(
    handles: &CommandHandles,
    thread_key: &ThreadKey,
    chat_type: &str,
    message_id: &str,
    skills: &[crate::backend::SkillInfo],
    error: Option<&str>,
) -> Result<()> {
    let card = super::picker::build_skill_card(thread_key, skills, "", 1, chat_type, message_id, error);
    let fallback = skill_list_text(skills, error);
    reply_card_or_text(handles, message_id, &card, "skill card", &fallback).await?;
    Ok(())
}

/// Reply one card under `message_id`, degrading to `fallback_text` when Feishu
/// rejects it (spec #652, ticket #655): a rejected card must not leave the
/// command dead — the same refusal-degrades-to-text shape `send_help_card`'s
/// `help_text()` fallback uses, shared by the `/skill` picker and the
/// loaded-skill card. The reply failure itself is not surfaced: the fallback is
/// the command's recovery.
async fn reply_card_or_text(
    handles: &CommandHandles,
    message_id: &str,
    card: &serde_json::Value,
    label: &str,
    fallback_text: &str,
) -> Result<()> {
    if let Err(e) = handles.flow.platform.reply_card(message_id, card).await {
        tracing::warn!("{label} failed ({}), falling back to text", e);
        handles
            .flow
            .platform
            .reply_text(message_id, fallback_text)
            .await?;
    }
    Ok(())
}

/// The plain-text fallback for [`send_skill_card`]: the same skills one line
/// each (`id`, name, description where present) plus the command syntax, with
/// the same leading error line an unknown id would show. Mirrors
/// [`crate::bridge::command::help_text`]'s role for the `/help` card. Bounded as
/// a whole (spec #652, ticket #655): each field is clipped to the picker's
/// row-label budget, at most [`super::skill::MAX_FALLBACK_SKILLS`] rows are
/// listed (with an elision count), and the whole text is capped.
fn skill_list_text(skills: &[crate::backend::SkillInfo], error: Option<&str>) -> String {
    let mut text = String::new();
    if let Some(error) = error {
        text.push_str(error);
        text.push('\n');
    }
    if skills.is_empty() {
        text.push_str("（没有可用技能）\n");
    } else {
        text.push_str("🧩 可用技能：\n");
        let shown = skills.len().min(super::skill::MAX_FALLBACK_SKILLS);
        for skill in &skills[..shown] {
            let id = super::truncate_md(&skill.id, super::picker::SKILL_ROW_LABEL_CHARS);
            let name = super::truncate_md(&skill.name, super::picker::SKILL_ROW_LABEL_CHARS);
            match super::picker::skill_description_line(skill) {
                Some(description) => {
                    let description = super::truncate_md(description, super::picker::SKILL_ROW_LABEL_CHARS);
                    text.push_str(&format!("- `{id}` {name} — {description}\n"));
                }
                None => text.push_str(&format!("- `{id}` {name}\n")),
            }
        }
        if skills.len() > shown {
            text.push_str(&format!("…（共 {} 个技能，已截断）\n", skills.len()));
        }
    }
    text.push_str("用法：`#<id>`");
    super::truncate_md(&text, super::skill::FALLBACK_TEXT_CHARS)
}

/// Reply the dedicated loaded-skill card (spec #652, ticket #655, acceptance
/// reversal) under the user's `/skill` message: one folded
/// `🧩 已加载技能：<name>` panel per distinct loaded skill, the body the skill's
/// own markdown from the skill-list read's `content`. `error`, when set, leads
/// the card — a mixed dispatch that resolved SOME ids but not others names the
/// unknown ones here, so nothing is dropped silently. Both the typed command
/// and a picker-row tap go through here, so the feedback is identical. A
/// rejected card must not leave the command dead, so — like
/// [`send_skill_card`] — the failure degrades to a plain-text listing of the
/// same skills.
pub(crate) async fn send_loaded_skill_card(
    handles: &CommandHandles,
    message_id: &str,
    skills: &[crate::backend::SkillInfo],
    error: Option<&str>,
) -> Result<()> {
    let card = super::skill::build_loaded_skill_card(skills, error);
    let fallback = loaded_skill_text(skills, error);
    reply_card_or_text(handles, message_id, &card, "loaded-skill card", &fallback).await?;
    Ok(())
}

/// The plain-text fallback for [`send_loaded_skill_card`]: the same leading error
/// line the card would show, then one section per loaded skill — the
/// `🧩 已加载技能：<name>` title AND its body, exactly what the card fold shows
/// (both consume [`super::skill::plan_skill_folds`] and
/// [`super::skill::fold_body`], so they never disagree on the extent). The
/// overflow past [`super::skill::SKILL_FOLD_MAX`] is named in one elision line,
/// mirroring the card's summary fold, so the degraded text is bounded as a
/// whole.
fn loaded_skill_text(skills: &[crate::backend::SkillInfo], error: Option<&str>) -> String {
    let mut text = String::new();
    if let Some(error) = error {
        text.push_str(error);
        text.push('\n');
    }
    let (allowed, hidden) = super::skill::plan_skill_folds(skills);
    for (skill, allowed) in skills.iter().zip(&allowed) {
        let name = super::truncate_md(&skill.name, super::picker::SKILL_ROW_LABEL_CHARS);
        text.push_str(&format!("🧩 已加载技能：{name}\n"));
        text.push_str(&super::skill::fold_body(skill, *allowed));
        text.push('\n');
    }
    if hidden > 0 {
        text.push_str(&format!("…等 {hidden} 个技能未列出\n"));
    }
    super::truncate_md(text.trim_end(), super::skill::FALLBACK_TEXT_CHARS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::session::PendingEntry;
    use crate::bridge::test_support::*;

    fn key() -> crate::config::ThreadKey {
        crate::config::ThreadKey::new("chat_1".into(), "chat_1".into())
    }

    /// `dir_card_data` derives Recent Directories from the shared store:
    /// children and archived sessions excluded, deduped by directory (keeping
    /// the latest activity), sorted most-recent-first.
    #[tokio::test]
    async fn dir_card_data_dedupes_sorts_and_filters() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut backend = MockBackend::new(realistic_parts());
        let mut child = list_session("ses_child", "子任务", "/work/a", 999);
        child.parent_id = Some("ses_root".into());
        let mut archived = list_session("ses_arch", "归档", "/work/arch", 888);
        archived.time = Some(crate::backend::types::SessionTime {
            created: 1,
            updated: 888,
            archived: Some(1),
        });
        backend.given_sessions(vec![
            list_session("ses_a1", "A1", "/work/a", 100),
            list_session("ses_b", "B", "/work/b", 200),
            list_session("ses_a2", "A2", "/work/a", 300),
            child,
            archived,
        ]);
        let (app, _platform) = build_app(cfg, backend).await;

        let (dirs, current) = dir_card_data(&app.command_handles(), &key(), "").await;
        assert_eq!(dirs, vec!["/work/a".to_string(), "/work/b".to_string()]);
        assert_eq!(current, None);
    }

    /// ADR-0051: a non-empty keyword narrows the union to paths — matching is
    /// case-insensitive, whitespace-token AND, and path-only (a session title
    /// never matches). The current directory participates: a non-matching
    /// keyword drops its row, but it is still reported as current so a match
    /// would be marked.
    #[tokio::test]
    async fn dir_card_data_filters_paths_by_token_and() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut backend = MockBackend::new(realistic_parts());
        backend.given_sessions(vec![
            list_session("ses_a", "项目A", "/work/a", 100),
            list_session("ses_b", "项目B", "/work/b", 200),
            list_session("ses_c", "登录重写", "/other/auth", 300),
        ]);
        let (app, _platform) = build_app(cfg, backend).await;

        // Token AND: "work a" matches /work/a, not /work/b; "AUTH" matches
        // case-insensitively; a session title is not a haystack.
        let (dirs, _) = dir_card_data(&app.command_handles(), &key(), "work a").await;
        assert_eq!(dirs, vec!["/work/a".to_string()]);
        let (dirs, _) = dir_card_data(&app.command_handles(), &key(), "AUTH").await;
        assert_eq!(dirs, vec!["/other/auth".to_string()]);
        let (dirs, _) = dir_card_data(&app.command_handles(), &key(), "项目A").await;
        assert!(dirs.is_empty(), "session titles are not matched: {dirs:?}");

        // The current directory participates in filtering.
        seed_session(&app, "ses_a", "/work/a").await;
        let (dirs, current) = dir_card_data(&app.command_handles(), &key(), "b").await;
        assert_eq!(dirs, vec!["/work/b".to_string()]);
        assert_eq!(current.as_deref(), Some("/work/a"));

        // An empty keyword keeps the whole union.
        let (dirs, _) = dir_card_data(&app.command_handles(), &key(), "").await;
        assert_eq!(dirs.len(), 3);
    }

    /// Directories cola has mapped are unioned in after the session-derived ones
    /// (deduped, most recently mapped first): they survive the server-side deletion
    /// or archival that drops a directory's last session — the exact case the
    /// shared store stops reporting it.
    #[tokio::test]
    async fn dir_card_data_unions_store_directories_dropped_by_the_server() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut backend = MockBackend::new(realistic_parts());
        let mut archived = list_session("ses_arch", "归档", "/work/arch", 888);
        archived.time = Some(crate::backend::types::SessionTime {
            created: 1,
            updated: 888,
            archived: Some(1),
        });
        backend.given_sessions(vec![list_session("ses_a", "A", "/work/a", 100), archived]);
        let (app, _platform) = build_app(cfg, backend).await;

        // /work/a is mapped too (dedup), /work/gone's session was deleted
        // server-side, /work/arch's only session is archived (filtered out).
        seed_session(&app, "ses_a", "/work/a").await;
        seed_session(&app, "ses_gone", "/work/gone").await;
        seed_session(&app, "ses_arch", "/work/arch").await;

        let (dirs, current) = dir_card_data(&app.command_handles(), &key(), "").await;
        assert_eq!(
            dirs,
            vec![
                "/work/a".to_string(),
                "/work/arch".to_string(),
                "/work/gone".to_string()
            ],
            "server-derived dirs first, then cola's own, most recently mapped first"
        );
        assert_eq!(current, Some("/work/arch".to_string()));
    }

    /// An empty session list (fresh or swapped store, or a failed fetch — the
    /// `unwrap_or_default`) still renders the directories cola has mapped instead
    /// of the empty-state hint.
    #[tokio::test]
    async fn dir_card_data_shows_store_directories_with_an_empty_session_list() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let (app, _platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

        seed_session(&app, "ses_x", "/work/x").await;

        let (dirs, current) = dir_card_data(&app.command_handles(), &key(), "").await;
        assert_eq!(dirs, vec!["/work/x".to_string()]);
        assert_eq!(current, Some("/work/x".to_string()));
    }

    /// A Pending Session's directory has no server session and no mapping entry
    /// (ADR-0041), so neither source carries it; the card still has to render it
    /// (as `当前`) instead of falling back to the empty-state hint.
    #[tokio::test]
    async fn dir_card_data_shows_a_pending_only_directory() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let (app, _platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

        seed_pending(&app, PendingEntry::new(key(), "/work/pending")).await;

        let (dirs, current) = dir_card_data(&app.command_handles(), &key(), "").await;
        assert_eq!(dirs, vec!["/work/pending".to_string()]);
        assert_eq!(current, Some("/work/pending".to_string()));
    }

    /// The `/dir` card's 当前 directory follows the pending, not the superseded
    /// active session — and the pending's directory is carried on the card even
    /// though no server session or mapping exists for it yet (ADR-0041).
    #[tokio::test]
    async fn dir_card_current_reads_pending() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut backend = MockBackend::new(realistic_parts());
        backend.given_sessions(vec![list_session("ses_a", "A", "/work/a", 100)]);
        let (app, _platform) = build_app(cfg, backend).await;

        seed_pending(&app, PendingEntry::new(key(), "/work/pending")).await;

        let (dirs, current) = dir_card_data(&app.command_handles(), &key(), "").await;
        assert_eq!(
            dirs,
            vec!["/work/pending".to_string(), "/work/a".to_string()],
            "the current directory is never missing from the card"
        );
        assert_eq!(current.as_deref(), Some("/work/pending"));
    }

    /// The `/switch` card's current-directory scope follows the pending, and no
    /// row is marked active — the pending is not a Session (ADR-0041); the
    /// superseded session stays mapped and switchable.
    #[tokio::test]
    async fn switch_card_current_reads_pending_and_marks_no_active_row() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut backend = MockBackend::new(realistic_parts());
        backend.given_sessions(vec![
            list_session("ses_a", "A", "/work/a", 100),
            list_session("ses_p", "P", "/work/pending", 200),
        ]);
        let (app, _platform) = build_app(cfg, backend).await;
        seed_entry(
            &app,
            crate::config::SessionEntry::new(key(), "ses_old", "/work/a"),
        )
        .await;
        seed_pending(&app, PendingEntry::new(key(), "/work/pending")).await;

        let (shown, active_id, mapped_ids, scope, current_dir) =
            switch_card_data(&app.command_handles(), &key(), "", SwitchScope::All).await;

        assert_eq!(shown.len(), 2);
        assert_eq!(current_dir.as_deref(), Some("/work/pending"));
        assert!(active_id.is_none(), "a pending means no active session");
        assert_eq!(mapped_ids, vec!["ses_old".to_string()]);
        assert_eq!(scope, SwitchScope::All);
    }
}
