//! The Error card's 「🔄 重试」 click (spec #391).

use std::sync::Arc;

use tokio::time::Instant;
use tracing::Instrument;

use crate::bridge::card_action::VerbFuture;
use crate::bridge::handler::{App, CardActionResult, RecoveryClick};
use crate::bridge::span;
use crate::feishu::card::CardState;

/// Re-submit a failed prompt from the error-card "retry" button (spec #391).
/// The card callback must ack within 3s, so the click claims the retry (the
/// atomic double-click guard behind the Turn interface), spawns the decision
/// pipeline and returns a "retrying" toast immediately; the pipeline then reads
/// the session's status and transcript once, decides, and either re-attaches
/// the card to a run that is still alive, or marks the failed card `Retried`
/// and submits the prompt onto a NEW card below it.
///
/// The claim is released when nothing is submitted and nothing is re-attached
/// (a session/thread that vanished, or a busy run with no anchor to follow), so
/// a later click can try again.
pub(crate) fn run<'a>(
    app: &'a Arc<App>,
    value: &'a serde_json::Value,
    _deadline: Instant,
    _callback_started: Instant,
) -> VerbFuture<'a> {
    Box::pin(async move {
        let RecoveryClick {
            recovery,
            thread_key,
            directory,
            handles,
        } = app.claim_recovery_click(value, CardState::Error, "retry").await?;
        let app = Arc::clone(app);
        let span = span::turn(&recovery.session_id, &thread_key, None);
        tokio::spawn(
            async move {
                app.run_retry(handles, recovery, thread_key, directory).await;
            }
            .instrument(span),
        );
        Some(CardActionResult {
            card: None,
            toast: Some("正在重试...".to_string()),
        })
    })
}
