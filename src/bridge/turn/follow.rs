//! The out-of-turn drain follow (#284).
//!
//! When a Turn's post-prompt drain reaches its bound while the session is
//! still running (a Supplement's continuation work outliving the 10-minute
//! budget — typically `task` subagents), the Turn is over but its card is not:
//! finalizing from that snapshot stamps `✅ 完成` on a card whose tool panels
//! still say `⏳` and freezes everything after the bound. The released Turn
//! therefore hands the SAME accumulator and card chain to this follow, which
//! keeps polling and rendering them until the session reports a non-busy
//! status, then finalizes Done — or Error when its own ceiling is reached
//! first, never an eternal spinner and never Done under a running panel.
//!
//! The follow runs out of turn: the inflight guard is already released, so a
//! message arriving meanwhile starts a normal new Turn, which replaces the
//! accumulator and ends the follow on its next tick (the same replacement
//! guard the external render loop uses). `/stop` still ends it promptly via
//! the sticky stopped-session marker.

use std::time::Duration;

use tracing::Instrument;

use crate::backend::TurnAnchor;
use crate::bridge::handles::TurnHandles;
use crate::bridge::span;
use crate::config::ThreadKey;
use crate::opencode::types::SessionStatus;

use super::Turn;

/// What the follow records when its ceiling is reached with the session still
/// running: the card ends Error (with its retry button), never Done under a
/// running tool panel.
const CEILING_ERROR: &str = "会话超过跟进时限仍在运行，已停止更新。";

/// Spawn the out-of-turn follow for a turn whose drain bound was reached with
/// the session still running. The caller has already released the inflight
/// guard; `anchor` is the accumulator's identity (the message id together
/// with its server time — one fact) and `started_at` the original turn's
/// start, so the follow's completion notice keeps the long-task threshold
/// measuring the whole run.
pub(super) fn spawn(
    handles: &TurnHandles,
    session_id: String,
    thread_key: ThreadKey,
    directory: String,
    started_at: std::time::Instant,
    anchor: TurnAnchor,
) {
    let handles = handles.clone();
    let poll_ms = handles.config.render_poll_ms();
    let ceiling_ms = handles.config.follow_timeout_ms();
    // A spawn inherits no span: instrument the follow with the session's own
    // `turn` span (ADR-0048), rooted like the render poll's.
    let span = span::turn(&session_id, &thread_key, None);
    tokio::spawn(
        async move {
            run(
                handles, session_id, directory, started_at, anchor, poll_ms, ceiling_ms,
            )
            .await;
        }
        .instrument(span),
    );
}

/// The follow loop. One tick: sleep, bail if the accumulator was replaced,
/// finalize promptly on `/stop`, read and render the session, then let the
/// session's own status decide — Busy/Retry keeps it alive, and so does a
/// still-live Tool Panel (an idle status with a `⏳` panel is not completion).
/// Every path that is not a finalization falls through to the ONE ceiling
/// check: at the ceiling the card ends Error.
async fn run(
    handles: TurnHandles,
    session_id: String,
    directory: String,
    started_at: std::time::Instant,
    anchor: TurnAnchor,
    poll_ms: u64,
    ceiling_ms: u64,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(ceiling_ms);
    loop {
        tokio::time::sleep(Duration::from_millis(poll_ms)).await;
        // The accumulator was replaced (a new Turn, or an external arming):
        // the follow no longer owns the card. Exit without touching anything.
        // The FULL anchor is the identity: another turn's message can share
        // this one's millisecond, and mutating its card would be a hijack.
        if Turn::armed_turn_anchor(&handles.cards, &session_id)
            .await
            .as_ref()
            != Some(&anchor)
        {
            return;
        }
        // `/stop` aborted this session's run: no answer is coming, so finalize
        // promptly instead of waiting out the ceiling (the drain's own stop
        // rule). One last render reconciles the abort's tool states before the
        // card goes Done, exactly as `finish` does on its stop path.
        if handles.waits.stopped_sessions.lock().await.contains(&session_id) {
            if let Some(Ok(transcript)) = crate::bridge::bounded_call(
                "drain follow transcript",
                super::drain_request_timeout(deadline),
                handles.backend.transcript(&session_id),
            )
            .await
            {
                Turn::render_and_flush(
                    &handles.cards,
                    &handles.sessions,
                    &handles.backend,
                    &session_id,
                    &transcript,
                )
                .await;
            }
            Turn::finalize_done(&handles.cards, &session_id).await;
            super::send_completion_notice(&handles, &session_id, started_at).await;
            tracing::info!("drain follow: session {} stopped; finalized", session_id);
            return;
        }
        match crate::bridge::bounded_call(
            "drain follow transcript",
            super::drain_request_timeout(deadline),
            handles.backend.transcript(&session_id),
        )
        .await
        {
            Some(Ok(transcript)) => {
                // Stream the parts into the SAME card (the accumulator this
                // turn has been rendering into all along); `None` means it is
                // gone.
                if Turn::render_and_flush(
                    &handles.cards,
                    &handles.sessions,
                    &handles.backend,
                    &session_id,
                    &transcript,
                )
                .await
                .is_none()
                {
                    return;
                }
                // The session's own status decides the end — never the mere
                // presence of a terminal part.
                match crate::bridge::bounded_call(
                    "drain follow session status",
                    super::drain_request_timeout(deadline),
                    handles.backend.session_status(&session_id, Some(&directory)),
                )
                .await
                {
                    // Still running: keep rendering (the ceiling is the only
                    // way out while it stays busy).
                    Some(Ok(Some(SessionStatus::Busy | SessionStatus::Retry))) => {}
                    // Non-busy: finalize Done — but never over a panel still
                    // marked live (`⏳`). A crash-orphaned tool can leave one
                    // behind on an idle session; it waits for the ceiling's
                    // Error rather than a false `✅`.
                    Some(Ok(_)) => {
                        let live_panels = Turn::has_live_tools(&handles.cards, &session_id).await;
                        if !live_panels {
                            Turn::finalize_done(&handles.cards, &session_id).await;
                            super::send_completion_notice(&handles, &session_id, started_at).await;
                            tracing::info!("drain follow: session {} idle; finalized", session_id);
                            return;
                        }
                    }
                    Some(Err(e)) => tracing::warn!("drain follow session status: {}", e),
                    // The bounded call timed out: the deadline is at hand.
                    None => {}
                }
            }
            Some(Err(e)) => tracing::warn!("drain follow transcript: {}", e),
            None => {}
        }
        if tokio::time::Instant::now() >= deadline {
            finalize_ceiling(&handles, &session_id, started_at).await;
            return;
        }
    }
}

/// The ceiling exit: the session is still busy (or unreadable) at the follow's
/// bound, so the card ends Error with the reason — never Done under a running
/// panel, never an eternal spinner.
async fn finalize_ceiling(handles: &TurnHandles, session_id: &str, started_at: std::time::Instant) {
    Turn::finalize_error(&handles.cards, session_id, CEILING_ERROR).await;
    super::send_completion_notice(handles, session_id, started_at).await;
    tracing::info!(
        "drain follow: session {} still running at the ceiling; finalized Error",
        session_id
    );
}
