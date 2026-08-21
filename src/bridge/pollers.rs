use std::sync::Arc;

use crate::bridge::handler::App;

/// Where to deliver a permission/question card.
pub enum CardTarget {
    /// Reply to a specific message (an in-flight prompt's streaming card).
    ReplyTo(String),
    /// Send into a chat.
    Chat(String),
}

/// Resolve where a card for `session_id` should be delivered.
///
/// Sessions cola created map to a Feishu chat directly. Sub-task sessions
/// (created by the `task` tool) are NOT in cola's SessionStore, so when the
/// direct lookup fails we walk up the parent chain (via `session_info`) until
/// a session cola knows is found. The directory is passed through so the
/// parent session is looked up in the same server instance.
pub(crate) async fn resolve_card_target(
    app: &Arc<App>,
    session_id: &str,
    directory: &str,
) -> Option<CardTarget> {
    let mut current = session_id.to_string();
    for _ in 0..8 {
        // In-flight prompt for this session → reply to its streaming card.
        let reply_to = {
            let accs = app.accumulators.lock().await;
            accs.get(&current).and_then(|a| a.reply_to_message_id.clone())
        };
        if let Some(msg_id) = reply_to {
            return Some(CardTarget::ReplyTo(msg_id));
        }
        // Session mapped to a chat → send into the chat.
        let chat = { app.sessions.lock().await.chat_for_session(&current) };
        if let Some(chat_id) = chat {
            return Some(CardTarget::Chat(chat_id));
        }
        // Unknown session → walk up to its parent (sub-task child sessions).
        let info = app.opencode.session_info(&current, Some(directory)).await.ok()?;
        let parent = info.parent_id.filter(|p| p != &current)?;
        current = parent;
    }
    None
}

/// Find the session whose streaming card an inline permission/question should
/// be shown on: `session_id` itself when it has a live accumulator, else the
/// nearest ancestor that does. Sub-task child sessions carry their own id and
/// have no accumulator of their own, so the parent chain is walked (like
/// `resolve_card_target`); the host's card then carries the child's buttons,
/// and clicking them still replies to the actual (child) session via `directory`.
pub(crate) async fn inline_host_session(
    app: &Arc<App>,
    session_id: &str,
    directory: Option<&str>,
) -> Option<String> {
    let mut current = session_id.to_string();
    for _ in 0..8 {
        {
            let accs = app.accumulators.lock().await;
            if accs.contains_key(&current) {
                return Some(current);
            }
        }
        let Ok(info) = app.opencode.session_info(&current, directory).await else {
            return None;
        };
        let parent = info.parent_id.filter(|p| p != &current)?;
        current = parent;
    }
    None
}
