//! The Turn card's 「🔍 查看完整变化」 click (spec #797, ticket #801).
//!
//! An action-less click: the payload carries `op` and no `action`, which is why
//! the registry selects it by `op` alone. The dispatch's
//! [`Gate::Diffs`](crate::bridge::card_action::Gate::Diffs) decides whether the
//! surface exists — a card persisted under a client that had the surface may be
//! clicked after a reattach, so the verb omits the read rather than guessing.

use std::sync::Arc;

use tokio::time::Instant;

use crate::bridge::card_action::VerbFuture;
use crate::bridge::handler::{App, CardActionResult, TurnDiffClick};

/// The click must ack within 3s while the fetch + HTML upload cannot fit it,
/// so it claims the Turn, spawns the delivery and returns a Toast immediately.
/// The in-process guard is keyed by `(session_id, from)` — the card's exact
/// Turn identity — and is bounded like the File Message once-guard: a later
/// click is a no-op (「已发送」, or the ambiguous-send wording) and never
/// generates or uploads again. Distinct Turns are distinct keys.
pub(crate) fn run<'a>(
    app: &'a Arc<App>,
    value: &'a serde_json::Value,
    _deadline: Instant,
    _callback_started: Instant,
) -> VerbFuture<'a> {
    Box::pin(async move {
        let session_id = value
            .get("session_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let from = value
            .get("from")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let directory = value
            .get("directory")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let message_id = value
            .get("open_message_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if session_id.is_empty() || message_id.is_empty() {
            return Some(CardActionResult {
                card: None,
                toast: Some("无效操作".to_string()),
            });
        }
        let key = (session_id.clone(), from.clone());
        if let Some(toast) = app.turn_diff_state_toast(&key) {
            return Some(CardActionResult {
                card: None,
                toast: Some(toast.to_string()),
            });
        }
        let Some(claim) = app.claim_turn_diff(&key) else {
            return Some(CardActionResult {
                card: None,
                toast: Some("正在生成完整 diff…".to_string()),
            });
        };
        let click = TurnDiffClick {
            session_id,
            from,
            directory,
            message_id,
        };
        let app = Arc::clone(app);
        tokio::spawn(async move {
            let _claim = claim;
            app.run_turn_diff_delivery(click).await;
        });
        Some(CardActionResult {
            card: None,
            toast: Some("正在生成完整 diff…".to_string()),
        })
    })
}
