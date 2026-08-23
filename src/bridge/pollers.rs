use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::bridge::handler::{App, CardActionResult};

/// How often the server-reconnect loop rescans `/proc` for a changed server.
const RECONNECT_POLL_INTERVAL_SECS: u64 = 5;

/// Independent poller: detects when the OpenCode server cola is attached to has
/// been restarted/replaced (another tool like OpenChamber manages it, so its
/// pid, port and password can change under us), and re-points the client at the
/// new server.
///
/// Without this, cola keeps hammering the dead port and every request 502s —
/// messages arrive on the WS but prompts/permissions never work again until a
/// manual cola restart. Started once at App startup.
pub(crate) async fn reconnect_poll_loop(app: &Arc<App>) -> crate::error::Result<()> {
    let mut current = app.opencode.base_url();
    loop {
        tokio::time::sleep(tokio::time::Duration::from_secs(RECONNECT_POLL_INTERVAL_SECS)).await;
        let candidates = crate::bridge::discovery::scan_processes();
        let Some(server) = crate::bridge::discovery::select_server(&candidates, None) else {
            continue;
        };
        let url = format!("http://localhost:{}", server.port);
        if url == current {
            continue;
        }
        tracing::warn!("OpenCode server changed ({} -> {}); reconnecting", current, url);
        if let Err(e) = app.opencode.reconnect(&url, &server.password).await {
            tracing::warn!("reconnect to {} failed: {}", url, e);
            continue; // keep the old endpoint; retry next tick
        }
        current = url;
    }
}

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

/// A JSON 2.0 result card for a card-action response (must stay 2.0 to remain
/// update-compatible with the 2.0 interactive cards — Feishu err 200830).
/// Shared by the permission and question flows (and the retry action).
pub(crate) fn result_card(title: &str, template: &str, body: &str) -> CardActionResult {
    CardActionResult {
        card: Some(serde_json::json!({
            "schema": "2.0",
            "config": { "wide_screen_mode": true },
            "header": { "title": { "tag": "plain_text", "content": title }, "template": template },
            "body": { "elements": [ { "tag": "markdown", "content": body } ] }
        })),
        toast: None,
    }
}

/// Mark permission/question cards as "already handled" when the underlying
/// request disappeared without cola answering it (another client resolved it).
/// The stale card keeps the original request description so the user can see
/// what was handled. Shared by the permission and question flows.
pub(crate) async fn mark_stale_cards(
    app: &Arc<App>,
    pending: &std::collections::HashSet<String>,
    sent: &Arc<Mutex<HashMap<String, (String, String)>>>,
    kind: &str,
) {
    let stale: Vec<(String, String, String)> = {
        let sent_map = sent.lock().await.clone();
        let answered = app.answered_requests.lock().await;
        sent_map
            .into_iter()
            .filter(|(rid, _)| !pending.contains(rid) && !answered.contains(rid))
            .map(|(rid, (mid, desc))| (rid, mid, desc))
            .collect()
    };
    for (rid, mid, desc) in stale {
        sent.lock().await.remove(&rid);
        let card = crate::feishu::card::build_resolved_elsewhere_card(kind, &desc);
        if let Err(e) = app.feishu.update_message(&mid, &card).await {
            tracing::warn!("mark stale {} card {}: {}", kind, rid, e);
        } else {
            tracing::info!("Marked stale {} card {} as handled", kind, rid);
        }
    }
}
