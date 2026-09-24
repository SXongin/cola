//! The command card forms (spec #298, ticket D): the state access and delivery
//! around the command cards, joining the structured-input → card-JSON builders
//! the card layer already owns.
//!
//! The JSON builders keep their card families and their contract — structured
//! inputs in, card JSON out: [`super::session::build_switch_card`] and
//! [`super::session::build_dir_card`], [`super::picker::build_agent_card`],
//! [`super::picker::build_model_provider_cards`],
//! [`super::picker::build_think_card`],
//! [`super::picker::build_autoaccept_card`], and
//! [`super::help::build_help_card`]. This module is the form layer over them:
//! it fetches the state each builder takes as its input, hands it over, and
//! sends the result — the fetch → build → send path for the cards the slash
//! commands pop (`/switch`, `/dir`, `/agent`, `/model`, `/think`,
//! `/autoaccept`, `/help`). `bridge::command` keeps the parser, the dispatch
//! match and the text-command behavior.
//!
//! The fetch-and-shape functions ([`switch_card_data`], [`dir_card_data`],
//! [`agent_card`], [`think_card`], [`current_model_label`]) are shared with the
//! coordinator's card-ack refresh, which rebuilds a clicked card from the same
//! one source of truth.

use serde_json::Value;

use crate::bridge::command::{help_text, matches_keyword};
use crate::bridge::display::feishu_side_label;
use crate::bridge::handles::CommandHandles;
use crate::config::ThreadKey;
use crate::error::Result;

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
    Vec<crate::opencode::types::SessionListInfo>,
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
    let mut shown: Vec<crate::opencode::types::SessionListInfo> = sessions
        .into_iter()
        .filter(|s| {
            !s.is_child()
                && !s.time.as_ref().map(|t| t.is_archived()).unwrap_or(false)
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
/// replies with the card.
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
        current_dir.as_deref(),
        active_id.as_deref(),
        &mapped_ids,
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
        if s.is_child() || s.time.as_ref().map(|t| t.is_archived()).unwrap_or(false) {
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
/// `/switch` search box's rule (`matches_keyword`) applied to the path only —
/// `lower_keyword` is already lowercased, split on whitespace, and every token
/// must appear as a substring.
fn matches_directory(dir: &str, lower_keyword: &str) -> bool {
    let dir = dir.to_lowercase();
    lower_keyword.split_whitespace().all(|tok| dir.contains(tok))
}

/// Build and send the interactive `/dir` Recent Directories card. Renders the
/// deduped directory list (via `dir_card_data`) and replies with the card.
pub(crate) async fn send_dir_card(
    handles: &CommandHandles,
    thread_key: &ThreadKey,
    message_id: &str,
) -> Result<()> {
    let (dirs, current_dir) = dir_card_data(handles, thread_key, "").await;
    let card = super::session::build_dir_card(thread_key, &dirs, current_dir.as_deref(), "");
    handles.flow.platform.reply_card(message_id, &card).await?;
    Ok(())
}

/// The first agent in `GET /agent` order that the server would actually run as
/// its default: primary (not subagent) and visible (not hidden). Parity with
/// opencode's `Agent.Service` `defaultInfo` fallback — `agent.list()` already
/// sorts the configured default (or `build`) first, so the first primary
/// visible agent in that order IS the server default; no `/config` round trip
/// needed. Mirrors `defaultInfo`'s "first primary non-hidden agent".
fn server_default_agent(agents: &[crate::opencode::types::AgentInfo]) -> Option<String> {
    agents
        .iter()
        .find(|a| a.mode.as_deref() != Some("subagent") && a.hidden != Some(true))
        .map(|a| a.name.clone())
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

/// Send the `/model` provider-picker cards (ADR-0012, issue 05): step 1 of a
/// two-level provider → model flow, chunked so any provider count stays under
/// Feishu's card limits. The intro carries the CURRENT model
/// ([`current_model_label`]) so the user sees what a pick would replace.
pub(crate) async fn send_model_card(
    handles: &CommandHandles,
    thread_key: &ThreadKey,
    message_id: &str,
) -> Result<()> {
    let current = current_model_label(handles, thread_key).await;
    let providers = handles.flow.backend.list_models().await;
    let cards = super::picker::build_model_provider_cards(thread_key, &providers, current.as_deref());
    for card in cards {
        handles.flow.platform.reply_card(message_id, &card).await?;
    }
    Ok(())
}

/// The current model label the `/model` picker renders: `provider/model` from
/// the effective-model ladder (settings override → configured default →
/// server-recorded), plus `@variant` when the target set a `/think` level. The
/// target is the Pending Session when one exists, else the active SessionEntry
/// (ADR-0041). `None` when the thread has no target or no rung resolves — the
/// picker then omits its current-model line. Shared by the text send path and
/// the card-ack rebuild so both show one source of truth.
pub(crate) async fn current_model_label(handles: &CommandHandles, thread_key: &ThreadKey) -> Option<String> {
    let settings = handles.flow.sessions.session_settings(thread_key).await?;
    let (provider, model) = handles
        .flow
        .sessions
        .effective_model(&handles.flow.backend, &settings)
        .await?;
    let variant = settings
        .variant
        .as_deref()
        .map(|v| format!("@{v}"))
        .unwrap_or_default();
    Some(format!("{provider}/{model}{variant}"))
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
    let Some((provider, model)) = handles
        .flow
        .sessions
        .effective_model(&handles.flow.backend, &settings)
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
        .model_variants(&handles.flow.backend, &provider, &model)
        .await
        .unwrap_or_default();
    if variants.is_empty() {
        return (
            None,
            Some(format!("当前模型 `{provider}/{model}` 没有思考等级可选。")),
        );
    }
    let current = settings.variant.clone();
    let card = super::picker::build_think_card(thread_key, &provider, &model, current.as_deref(), &variants);
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
        archived.time = Some(opencode::types::SessionTime {
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
        archived.time = Some(opencode::types::SessionTime {
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
