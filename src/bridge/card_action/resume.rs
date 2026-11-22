//! The Unreceived card's 重新发起 click (ADR-0062, #437).

use std::sync::Arc;

use tokio::time::Instant;
use tracing::Instrument;

use crate::bridge::card_action::VerbFuture;
use crate::bridge::handler::{App, CardActionResult, RecoveryClick};
use crate::bridge::span;
use crate::feishu::card::CardState;

/// Handle the Unreceived card's 重新发起 action (ADR-0062, #437): on V2 the
/// click interrupts (a no-op when the Session is already idle) and resumes,
/// promoting the queued steer at the new run's start; on V1, which has no
/// resume endpoint, the action degrades to resubmitting the message as a
/// new Turn. Either way the already-admitted message is reused, never
/// retyped, and the transcript message is never duplicated.
///
/// The card callback must ack within 3s, so the click claims the action
/// (the same atomic double-click guard the Error card's retry uses), spawns
/// the pipeline and returns a toast immediately. A claim that cannot
/// proceed (no thread mapping, or a prompt already in flight) is released,
/// so a later press can try again.
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
        } = app
            .claim_recovery_click(value, CardState::Unreceived, "resume")
            .await?;
        let app = Arc::clone(app);
        let span = span::turn(&recovery.session_id, &thread_key, None);
        tokio::spawn(
            async move {
                app.run_resume(handles, recovery, thread_key, directory).await;
            }
            .instrument(span),
        );
        Some(CardActionResult {
            card: None,
            toast: Some("正在重新发起...".to_string()),
        })
    })
}
