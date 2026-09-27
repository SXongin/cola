//! The out-of-turn drain follow (#284, #386).
//!
//! When a Turn's post-prompt drain reaches its bound while the session is
//! still running (a Supplement's continuation work outliving the 10-minute
//! budget — typically `task` subagents), the Turn is over but its card is not:
//! finalizing from that snapshot stamps `✅ 完成` on a card whose tool panels
//! still say `⏳` and freezes everything after the bound. The released Turn
//! therefore hands the SAME accumulator and card chain to this follow, which
//! keeps polling and rendering them until the session reports a non-busy
//! status, then finalizes Done — or Error when the settled turn failed.
//!
//! The follow has NO total budget (#386): a readable run may take as long as
//! it takes, and a Permission/Question wait is unbounded by design — the card
//! header names the wait and the Instant Reminder nags, so a clock could only
//! turn a correct wait into a wrong error. The grace
//! (`turn_follow_grace_ms`) bounds exactly the two failure modes where nobody
//! can act:
//!
//! - **lost contact**: no tick in the grace produced a full read pair (both
//!   the transcript and the status answered) — a hung or wedged Backend. The
//!   card ends Error instead of spinning over a run cola cannot see.
//! - **an unreconcilable live panel**: a readable, non-busy session keeps a
//!   `⏳` panel (a crash-orphaned call) past the grace. Done would lie about
//!   the panel, so the card ends Error.
//!
//! The follow runs out of turn: the inflight guard is already released, so a
//! message arriving meanwhile starts a normal new Turn, which replaces the
//! accumulator and ends the follow on its next tick (the same replacement
//! guard the external render loop uses). `/stop` still ends it promptly via
//! the sticky stopped-session marker.

use std::time::Duration;

use tracing::Instrument;

use crate::backend::{SessionTranscript, TurnAnchor};
use crate::bridge::handles::TurnHandles;
use crate::bridge::span;
use crate::config::ThreadKey;
use crate::opencode::types::SessionStatus;

use super::Turn;

/// What the follow records when its reads stopped answering for the grace:
/// the card never sits on an eternal spinner over a Backend it cannot see.
const LOST_CONTACT_ERROR: &str = "与运行失去联系，已停止更新。";

/// What the follow records when a readable, non-busy session carries a live
/// Tool Panel past the grace (a crash-orphaned call): the card ends Error,
/// never Done over a `⏳` panel.
const STUCK_PANEL_ERROR: &str = "运行已结束但工具状态未收尾，已停止更新。";

/// The follow's injected timing knobs, bundled so the loop's signature stays
/// readable: the render poll cadence, the lost-contact / stuck-panel grace,
/// and the per-read bound.
#[derive(Clone, Copy)]
struct Timing {
    poll_ms: u64,
    grace_ms: u64,
    read_timeout_ms: u64,
}

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
    let timing = Timing {
        poll_ms: handles.config.render_poll_ms(),
        grace_ms: handles.config.follow_grace_ms(),
        read_timeout_ms: handles.config.follow_read_timeout_ms(),
    };
    // A spawn inherits no span: instrument the follow with the session's own
    // `turn` span (ADR-0048), rooted like the render poll's.
    let span = span::turn(&session_id, &thread_key, None);
    tokio::spawn(
        async move {
            run(handles, session_id, directory, started_at, anchor, timing).await;
        }
        .instrument(span),
    );
}

/// The follow loop. One tick: sleep, bail if the accumulator was replaced,
/// finalize promptly on `/stop`, read and render the session, then let the
/// session's own status decide — Busy/Retry keeps it alive (there is no total
/// budget), a readable non-busy session finalizes (Done, or the settled
/// failure's Error), and an idle session that keeps a `⏳` panel runs the
/// stuck-panel grace. A tick that did not read BOTH the transcript and the
/// status runs the lost-contact grace; every fully-answered tick resets it.
async fn run(
    handles: TurnHandles,
    session_id: String,
    directory: String,
    started_at: std::time::Instant,
    anchor: TurnAnchor,
    timing: Timing,
) {
    let grace = Duration::from_millis(timing.grace_ms);
    let mut last_contact = tokio::time::Instant::now();
    let mut stuck_since: Option<tokio::time::Instant> = None;
    loop {
        tokio::time::sleep(Duration::from_millis(timing.poll_ms)).await;
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
        // promptly instead of waiting out the grace (the drain's own stop
        // rule). One last render reconciles the abort's tool states before the
        // card goes Done, exactly as `finish` does on its stop path.
        if handles.waits.stopped_sessions.lock().await.contains(&session_id) {
            if let Some(Ok(transcript)) = crate::bridge::bounded_call(
                "drain follow transcript",
                timing.read_timeout_ms,
                handles.backend.transcript(&session_id),
            )
            .await
            {
                Turn::render_and_flush(
                    &handles.cards,
                    &handles.sessions,
                    &handles.backend,
                    &handles.requests,
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
        // The reads. BOTH must answer for the tick to count as contact: a
        // wedged transcript freezes the card's content and a wedged status
        // hides the end decision — each is exactly the "cannot see the run"
        // state the lost-contact grace exists for.
        let mut in_contact = true;
        let transcript = match crate::bridge::bounded_call(
            "drain follow transcript",
            timing.read_timeout_ms,
            handles.backend.transcript(&session_id),
        )
        .await
        {
            // Stream the parts into the SAME card (the accumulator this turn
            // has been rendering into all along); `None` means it is gone.
            Some(Ok(transcript)) => {
                if Turn::render_and_flush(
                    &handles.cards,
                    &handles.sessions,
                    &handles.backend,
                    &handles.requests,
                    &session_id,
                    &transcript,
                )
                .await
                .is_none()
                {
                    return;
                }
                Some(transcript)
            }
            Some(Err(e)) => {
                tracing::warn!("drain follow transcript: {}", e);
                in_contact = false;
                None
            }
            None => {
                in_contact = false;
                None
            }
        };
        let status = match crate::bridge::bounded_call(
            "drain follow session status",
            timing.read_timeout_ms,
            handles.backend.session_status(&session_id, Some(&directory)),
        )
        .await
        {
            Some(Ok(status)) => Some(status),
            Some(Err(e)) => {
                tracing::warn!("drain follow session status: {}", e);
                in_contact = false;
                None
            }
            None => {
                in_contact = false;
                None
            }
        };
        match status {
            // Still running: keep rendering. There is no total budget; the
            // grace below only watches for lost contact.
            Some(Some(SessionStatus::Busy | SessionStatus::Retry)) => {
                stuck_since = None;
            }
            // Non-busy: finalize — but never over a panel still marked live
            // (`⏳`). A crash-orphaned tool can leave one behind on an idle
            // session; it gets the grace to settle, then ends Error rather
            // than a false `✅`.
            //
            // The settled turn's own failure decides the ending (ADR-0056):
            // the failure lives on the newest assistant message, so a long
            // turn that ended in a provider failure must finalize Error, not
            // Done.
            Some(_) => {
                let live_panels = Turn::has_live_tools(&handles.cards, &session_id).await;
                match transcript.as_ref() {
                    Some(transcript) if !live_panels => {
                        match turn_failure(transcript, &anchor) {
                            Some(error) => {
                                Turn::finalize_error(&handles.cards, &session_id, &error).await;
                                super::send_completion_notice(&handles, &session_id, started_at).await;
                                tracing::info!(
                                    "drain follow: session {} failed; finalized Error",
                                    session_id
                                );
                            }
                            None => {
                                Turn::finalize_done(&handles.cards, &session_id).await;
                                super::send_completion_notice(&handles, &session_id, started_at).await;
                                tracing::info!("drain follow: session {} idle; finalized", session_id);
                            }
                        }
                        return;
                    }
                    // No fresh transcript: do not claim an ending from a read
                    // the follow could not make — the lost-contact grace owns
                    // that state.
                    Some(_) => {
                        let since = *stuck_since.get_or_insert_with(tokio::time::Instant::now);
                        if since.elapsed() >= grace {
                            finalize_fallback(&handles, &session_id, started_at, STUCK_PANEL_ERROR).await;
                            return;
                        }
                    }
                    None => stuck_since = None,
                }
            }
            // Unreadable status: observation is broken; the lost-contact
            // grace below decides. A stuck panel's continuity is broken too.
            None => stuck_since = None,
        }
        if in_contact {
            last_contact = tokio::time::Instant::now();
        } else if last_contact.elapsed() >= grace {
            finalize_fallback(&handles, &session_id, started_at, LOST_CONTACT_ERROR).await;
            return;
        }
    }
}

/// The failure the followed turn recorded, read from its settled transcript
/// through the same projection the Turn's own finalization uses
/// ([`crate::backend::TurnView::error`]: the NEWEST assistant message's
/// failure, so a recovered earlier step is not a failure).
fn turn_failure(transcript: &SessionTranscript, anchor: &TurnAnchor) -> Option<String> {
    transcript.turn_for_user(anchor).error
}

/// A grace exit: the run is either unreadable or carries a panel that will
/// never settle, so the card ends Error with the reason — never Done under a
/// running panel, never an eternal spinner.
async fn finalize_fallback(
    handles: &TurnHandles,
    session_id: &str,
    started_at: std::time::Instant,
    error: &str,
) {
    Turn::finalize_error(&handles.cards, session_id, error).await;
    super::send_completion_notice(handles, session_id, started_at).await;
    tracing::info!(
        "drain follow: session {} ended by the fallback; finalized Error",
        session_id
    );
}
