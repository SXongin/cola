//! The Waiting card's 「清理待确认任务」 click (spec #588, ticket #590).

use std::sync::Arc;

use tokio::time::Instant;
use tracing::Instrument;

use crate::bridge::card_action::VerbFuture;
use crate::bridge::handler::{App, CardActionResult};
use crate::bridge::span;

/// The user's exit for the ⚠️ 状态待确认 residue no machine can confirm. The
/// card callback must ack within 3s, so the click claims the cleanup (the same
/// atomic double-click guard the retry uses, on the waiting card), spawns the
/// pipeline and returns a "cleaning" toast immediately; the pipeline then reads
/// the session's transcript and runtime once, retires exactly the unconfirmed
/// live tasks it finds, and re-renders the waiting card.
pub(crate) fn run<'a>(
    app: &'a Arc<App>,
    value: &'a serde_json::Value,
    _deadline: Instant,
    _callback_started: Instant,
) -> VerbFuture<'a> {
    Box::pin(async move {
        let sid = value.get("session_id").and_then(|v| v.as_str()).unwrap_or("");
        if sid.is_empty() {
            return None;
        }
        let handles = app.turn_handles();
        if !crate::bridge::turn::Turn::claim_cleanup(&handles.cards, sid).await {
            tracing::warn!("cleanup: no waiting card with unconfirmed tasks for session {sid}");
            return None;
        }
        let (thread_key, directory) = {
            let sessions = app.sessions.lock().await;
            (
                sessions.thread_for_session(sid),
                sessions.directory_for_session(sid),
            )
        };
        let Some(thread_key) = thread_key else {
            crate::bridge::turn::Turn::release_cleanup_claim(&handles.cards, sid).await;
            tracing::warn!("cleanup: no thread mapped for session {sid}");
            return None;
        };
        let session_id = sid.to_string();
        let span = span::turn(&session_id, &thread_key, None);
        tokio::spawn(
            async move {
                App::run_cleanup(handles, session_id, directory).await;
            }
            .instrument(span),
        );
        Some(CardActionResult {
            card: None,
            toast: Some("正在清理...".to_string()),
        })
    })
}
