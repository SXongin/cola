//! The `/skill` picker's clicks (spec #652, tickets #654/#656; #664).
//!
//! `action: "skill"` carries two shapes: an `op: "search"`/`op: "page"`
//! rebuild, which re-reads the list and replaces the card in place, and a ROW
//! TAP, which carries the skill's exact id and dispatches it straight to
//! [`App::handle_skill_command`] — the same submit path a typed command takes,
//! minus the text parse. The id is NOT round-tripped as `/skill <id>` text:
//! V1's id IS the skill name, which can contain whitespace (`Implement Spec`),
//! and `/skill Implement Spec` would parse as two ids and resolve neither.

use std::sync::Arc;

use tokio::time::Instant;

use crate::bridge::card_action::VerbFuture;
use crate::bridge::handler::{App, CardActionResult, thread_key_from_value};
use crate::config::{ConversationKind, ThreadKey};

/// The `/skill` picker's callback (#664). A `search`/`page` op rebuilds the
/// card in place; anything else is a row tap. The click must ack within 3s
/// while a turn runs for the length of a model reply, so the submission is
/// spawned off the ack; the picker card stays put (the dedicated loaded-skill
/// card replies under the ORIGINAL user message, spec #655), and the
/// agent/autoaccept toasts already established that a picker does not replace
/// itself on a pick.
pub(crate) fn run<'a>(
    app: &'a Arc<App>,
    value: &'a serde_json::Value,
    _deadline: Instant,
    _callback_started: Instant,
) -> VerbFuture<'a> {
    Box::pin(async move {
        let op = value.get("op").and_then(|v| v.as_str()).unwrap_or("");
        if op == "search" || op == "page" {
            return Some(rebuild_skill_card(app, value).await);
        }
        let id = value.get("value").and_then(|v| v.as_str()).unwrap_or("");
        if id.is_empty() {
            return None;
        }
        let app = Arc::clone(app);
        let value = value.clone();
        let toast = format!("正在加载技能 {id}…");
        let id = id.to_string();
        tokio::spawn(async move {
            // The Chat/Topic kind the picker was sent in: every button carries
            // it next to chat_id/thread_id, since a click has no chat_type of
            // its own (see `build_skill_card`).
            let callback_string =
                |key: &str| value.get(key).and_then(|v| v.as_str()).unwrap_or("").to_string();
            let chat_id = callback_string("chat_id");
            // A non-topic key's `thread_id` IS its `chat_id` (ThreadKey), so
            // drop it: re-classifying must not mistake the top level for a
            // topic. A real topic's `thread_id` differs and is kept.
            let thread_id = value
                .get("thread_id")
                .and_then(|v| v.as_str())
                .filter(|id| *id != chat_id)
                .map(str::to_string);
            let chat_type = {
                let t = callback_string("chat_type");
                if t.is_empty() { "group".to_string() } else { t }
            };
            let requester = value
                .get("operator_open_id")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            // The loaded-skill card (and the turn's card) must reply under the
            // ORIGINAL user message the picker was sent under, exactly like a
            // typed dispatch — not under the picker card. The picker's buttons
            // carry that id (`reply_message_id`, see `build_skill_card`); a card
            // built before that field existed falls back to the picker's own
            // message.
            let reply_to = {
                let carried = callback_string("reply_message_id");
                if carried.is_empty() {
                    callback_string("open_message_id")
                } else {
                    carried
                }
            };
            let kind = ConversationKind::classify(&chat_type, thread_id.as_deref());
            let thread_key = kind.thread_key(&chat_id, thread_id.as_deref());
            app.handle_skill_command(
                thread_key,
                crate::bridge::IncomingMessage {
                    message_id: reply_to,
                    chat_id,
                    chat_type,
                    thread_id,
                    parent_id: None,
                    text: format!("#{id}"),
                    images: vec![],
                    files: vec![],
                    omitted: vec![],
                    requester_open_id: requester,
                },
                crate::bridge::command::SkillInvocation {
                    ids: vec![id],
                    picker_on_empty: true,
                },
            )
            .await;
        });
        Some(CardActionResult {
            card: None,
            toast: Some(toast),
        })
    })
}

/// Rebuild the `/skill` picker card in place for an `op: "search"` /
/// `op: "page"` callback (#664). The filter (keyword, page) rides the button
/// payload (ADR-0052); the skill list is re-read from the server — unlike
/// `/dir`, which reads the local store — scoped to the Chat/Topic's project
/// directory. Best-effort: a failed read degrades to the no-skills state.
/// The `chat_type` + `reply_message_id` the picker carries ride back onto the
/// fresh rows, so a later tap still replies the loaded-skill card under the
/// ORIGINAL user message (spec #655). No Lazy Start here: a rebuild never
/// submits a prompt, and the ack must stay fast.
async fn rebuild_skill_card(app: &Arc<App>, value: &serde_json::Value) -> CardActionResult {
    let thread_key = thread_key_from_value(value);
    let keyword = value
        .get("keyword")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    // A missing/garbage page reads as 1; the builder clamps an out-of-range
    // page to the last.
    let page = value.get("page").and_then(|v| v.as_u64()).unwrap_or(1).max(1) as usize;
    // The picker's own `chat_type`, absent only on the WS name-only
    // form-submit fallback. Defaulting to "group" is inert for routing:
    // `ConversationKind::thread_key` keys every non-topic kind on `chat_id`,
    // and a real topic is recognised by its distinct `thread_id`, not the
    // chat type — so the rebuilt rows route identically either way. Only the
    // tap's reply target degrades (below), the accepted fallback shape.
    let chat_type = value
        .get("chat_type")
        .and_then(|v| v.as_str())
        .filter(|t| !t.is_empty())
        .unwrap_or("group")
        .to_string();
    // The original user message the picker replied under; a card built
    // before this field existed (or the name-only form-submit fallback)
    // falls back to the picker card's own message.
    let reply_message_id = value
        .get("reply_message_id")
        .and_then(|v| v.as_str())
        .filter(|id| !id.is_empty())
        .or_else(|| value.get("open_message_id").and_then(|v| v.as_str()))
        .unwrap_or("")
        .to_string();
    let card = build_skill_card_for(app, &thread_key, &keyword, page, &chat_type, &reply_message_id).await;
    CardActionResult {
        card: Some(card),
        toast: None,
    }
}

/// Read the skills for `thread_key`'s project directory, narrow them by
/// `keyword` (id / name / description), and window them to `page` (#664).
async fn build_skill_card_for(
    app: &Arc<App>,
    thread_key: &ThreadKey,
    keyword: &str,
    page: usize,
    chat_type: &str,
    message_id: &str,
) -> serde_json::Value {
    let directory = app.command_handles().current_project_directory(thread_key).await;
    let skills = app
        .client
        .list_skills((!directory.is_empty()).then_some(directory.as_str()))
        .await;
    let filtered = crate::feishu::card::picker::filter_skills(&skills, keyword);
    crate::feishu::card::picker::build_skill_card(
        thread_key, &filtered, keyword, page, chat_type, message_id, None,
    )
}
