//! The `/agent` picker's pick and clear clicks (ADR-0020).

use std::sync::Arc;

use tokio::time::Instant;

use crate::bridge::card_action::VerbFuture;
use crate::bridge::handler::{App, CardActionResult, thread_key_from_value};
use crate::bridge::handles::{PickReply, classify_pick};

/// An `/agent` option pick: record the per-session override — and on a durable
/// generation (V2) switch the session's agent. Refreshes the card so the
/// current agent updates.
pub(crate) fn run<'a>(
    app: &'a Arc<App>,
    value: &'a serde_json::Value,
    _deadline: Instant,
    _callback_started: Instant,
) -> VerbFuture<'a> {
    Box::pin(async move { pick(app, value, false).await })
}

/// The dedicated `agent_clear` control, which on V2 switches to the server's
/// default agent id. Clearing is carried by the ACTION tag, never a value word
/// — an agent literally named like a clear verb stays selectable (ADR-0020), so
/// `clear` is a dispatch flag, not a value predicate.
pub(crate) fn run_clear<'a>(
    app: &'a Arc<App>,
    value: &'a serde_json::Value,
    _deadline: Instant,
    _callback_started: Instant,
) -> VerbFuture<'a> {
    Box::pin(async move { pick(app, value, true).await })
}

async fn pick(app: &Arc<App>, value: &serde_json::Value, clear: bool) -> Option<CardActionResult> {
    let core = &app.core;
    let picked = value
        .get("value")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let thread_key = thread_key_from_value(value);
    let agent = if clear { None } else { Some(picked.clone()) };
    let outcome = core.pick_agent(&thread_key, agent).await;
    match classify_pick(
        outcome,
        "Agent",
        crate::bridge::display::feishu_side_label(&thread_key),
    ) {
        PickReply::Applied { .. } => {}
        PickReply::Failed(text) => {
            return Some(CardActionResult {
                card: None,
                toast: Some(text),
            });
        }
    }
    let (card, _error) = crate::feishu::card::command::agent_card(&core.command_handles(), &thread_key).await;
    Some(CardActionResult {
        card,
        toast: Some(if clear {
            "已清除 Agent（回到服务器默认）".to_string()
        } else {
            format!("Agent: {picked}（下一条消息开始生效）")
        }),
    })
}
