//! The out-of-turn drain follow (#284, #386).
//!
//! When a Turn's post-prompt drain reaches its bound while the session is
//! still running (a Supplement's continuation work outliving the 10-minute
//! budget — typically `task` subagents), the Turn is over but its card is not:
//! finalizing from that snapshot stamps `✅ 完成` on a card whose tool panels
//! still say `⏳` and freezes everything after the bound. The released Turn
//! therefore hands the SAME accumulator and card chain to this follow.
//!
//! The follow is the shared out-of-turn settle loop ([`super::settle`]) under
//! this Turn's anchor — the same reads, the same no-total-budget graces and
//! the same settle decision — and adds the one fact only it owns: an ending is
//! the Turn's TRUE end (or its fallback Error), so it announces it with the
//! Completion Notice; the notice itself declines a card that is not at an
//! ending, so the waiting yield stays silent (ADR-0059).
//!
//! The follow runs out of turn: the inflight guard is already released, so a
//! message arriving meanwhile starts a normal new Turn, which replaces the
//! accumulator and ends the follow on its next tick (the loop's ownership
//! guard). `/stop` still ends it promptly in the stop terminal, never Done or
//! Error (#394).

use tracing::Instrument;

use crate::backend::TurnAnchor;
use crate::bridge::handles::TurnHandles;
use crate::bridge::span;
use crate::config::ThreadKey;

use super::{SettleTiming, settle};

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
    let timing = SettleTiming {
        poll_ms: handles.config.render_poll_ms(),
        read_timeout_ms: handles.config.follow_read_timeout_ms(),
        grace_ms: handles.config.follow_grace_ms(),
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

/// The follow's loop: the shared settle loop under the accumulator's anchor,
/// then the ending stamped on the card and the Turn announced. The notice is
/// sent for every ending (the helper itself declines a card that is not at an
/// ending, so a waiting yield stays silent) and reads the card's real terminal
/// for its copy (#394).
async fn run(
    handles: TurnHandles,
    session_id: String,
    directory: String,
    started_at: std::time::Instant,
    anchor: TurnAnchor,
    timing: SettleTiming,
) {
    let flow = handles.flow();
    let owns = settle::Ownership::TurnAnchor(anchor);
    let Some(ending) = settle::run(&flow, &session_id, &directory, timing, &owns, "drain follow").await
    else {
        return;
    };
    settle::stamp(&flow.cards, &session_id, &ending).await;
    super::send_completion_notice(&handles, &session_id, started_at).await;
}
