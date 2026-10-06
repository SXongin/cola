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
//! Completion Notice; the notice itself declines a disposition that is not a
//! true end, so the waiting yield stays silent (ADR-0059).
//!
//! The follow runs out of turn but holds the Session's inflight guard for its
//! whole window (ADR-0059): a message arriving meanwhile is a Supplement that
//! merges into the still-live chain (the Card Chain splits below it), and the
//! server-yield's busy read never sees a followed Session as idle. The guard
//! is released when the loop ends — before the ending is stamped — so the next
//! message is a normal new Turn. `/stop` still ends it promptly in the stop
//! terminal, never Done or Error (#394).

use tracing::Instrument;

use crate::backend::TurnAnchor;
use crate::bridge::handles::TurnHandles;
use crate::bridge::span;
use crate::config::ThreadKey;

use super::{SettleTiming, settle};

/// The fixture a follow inherits from the Turn it continues: whose card it
/// watches, where its reads route, and when the run started (the long-task
/// notice's clock). Grouped so [`spawn`]/[`run`] carry one fact bundle instead
/// of five positional arguments.
pub(super) struct FollowFacts {
    pub(super) session_id: String,
    pub(super) thread_key: ThreadKey,
    /// The session's working directory, for the settle loop's status reads.
    pub(super) directory: String,
    /// The original Turn's start; the retry re-attach passes "now" (the
    /// original start is no longer known there), so the long-task notice
    /// measures the stretch the follow actually covers. Also the unreceived
    /// watch's hint clock: the neutral waiting line (ADR-0062) appears once
    /// the follow grace has passed since the turn was submitted.
    pub(super) started_at: std::time::Instant,
    /// The accumulator's identity — the message id together with its server
    /// time, one fact: the loop's ownership guard and the settle decision's
    /// scope. `None` when the submitted message has not landed yet (the
    /// unreceived watch, ADR-0062): the loop then owns the card by chain
    /// identity and captures the anchor from the transcript the moment the
    /// message appears — or ends the card Unreceived when the session idles
    /// first.
    pub(super) anchor: Option<TurnAnchor>,
}

/// Take the Session's inflight guard for a follow window (ADR-0059).
/// Idempotent by design: the drain hand-off's Turn deliberately did not
/// release, so its call finds the guard already held, while the retry
/// re-attach acquires it here — either way the hand-off has no guard-free gap.
/// [`run`] hands the guard back when the loop ends.
pub(super) async fn inherit_guard(handles: &TurnHandles, session_id: &str) {
    handles.waits.inflight.lock().await.insert(session_id.to_string());
}

/// Spawn the out-of-turn follow for a turn whose drain bound was reached with
/// the session still running.
///
/// **Guard hand-off (ADR-0059):** the follow takes the Session's inflight
/// guard via [`inherit_guard`] BEFORE its task is spawned, so a caller must
/// never release between handing the card over and this call — the drain
/// hand-off's Turn deliberately does not release, and the retry re-attach has
/// no guard to release. The guard then covers the whole follow window: a
/// message arriving meanwhile is a Supplement merging into the still-live
/// chain, and the server-yield's busy read never sees a followed Session as
/// idle. [`run`] releases it when the window closes.
pub(super) async fn spawn(handles: &TurnHandles, facts: FollowFacts) {
    inherit_guard(handles, &facts.session_id).await;
    let handles = handles.clone();
    let timing = SettleTiming {
        poll_ms: handles.config.render_poll_ms(),
        read_timeout_ms: handles.config.follow_read_timeout_ms(),
        grace_ms: handles.config.follow_grace_ms(),
    };
    // A spawn inherits no span: instrument the follow with the session's own
    // `turn` span (ADR-0048), rooted like the render poll's.
    let span = span::turn(&facts.session_id, &facts.thread_key, None);
    tokio::spawn(
        async move {
            run(handles, facts, timing).await;
        }
        .instrument(span),
    );
}

/// What the follow's loop owns (ADR-0059, ADR-0062): the accumulator's Turn
/// anchor when the submitted message has landed — the renderer identity a new
/// Turn or an external arming replaces — or, when the message has not landed
/// yet (the unreceived watch), the card's chain identity, which survives a
/// Supplement split and dies with a new Turn. The watch carries the submitted
/// id the anchor capture looks for and the hint's deadline (the turn's start
/// plus the follow grace); the loop reads all three through the variant.
/// `None` — the card is gone, or carries no submitted message — means there is
/// nothing to watch; [`run`] then hands the guard back and stops.
async fn ownership(
    handles: &TurnHandles,
    facts: &FollowFacts,
    timing: SettleTiming,
) -> Option<settle::Ownership> {
    if let Some(anchor) = &facts.anchor {
        return Some(settle::Ownership::TurnAnchor(anchor.clone()));
    }
    // The unreceived watch: the card's chain identity and the accumulator's
    // submitted id, both read through the Turn accessors so the watch and
    // `capture_turn_anchor` look at the same facts. Absence is explicit — a
    // sentinel chain or an empty id could never match what the loop watches.
    let chain = super::Turn::chain_id(&handles.cards, &facts.session_id).await?;
    let submitted = super::Turn::submitted_message_id(&handles.cards, &facts.session_id).await?;
    Some(settle::Ownership::Unlanded {
        chain,
        submitted,
        hint_at: facts.started_at + std::time::Duration::from_millis(timing.grace_ms),
    })
}

/// The follow's loop: the shared settle loop under the accumulator's anchor
/// (or, before the submitted message lands, under the unreceived watch's
/// chain identity — ADR-0062), then the ending disposition applied through the
/// one shared, ownership-checked application (`Ownership::apply_if_held`) and
/// the Turn announced. The notice is sent for every ending (its own
/// classification declines one that is not a true end, so a waiting yield and
/// an Unreceived card stay silent) and carries the copy of the disposition the
/// loop applied (#394).
///
/// The loop owns the Session's guard for the whole run and hands it back as
/// soon as the loop ends — BEFORE the ending is applied. A message arriving at
/// the end boundary must be a normal new Turn (the waiting window is exactly
/// that state), never a Supplement racing a card that is about to be
/// finalized. By then the run is over, so nothing is lost: the ownership
/// re-check and the stamp share ONE cards lock, so the loop stamps only a
/// chain it still owns — a new Turn that slipped into the released moment
/// replaced the accumulator, and stamping its live card with the old ending
/// would be a lie.
async fn run(handles: TurnHandles, facts: FollowFacts, timing: SettleTiming) {
    let Some(owns) = ownership(&handles, &facts, timing).await else {
        // The card vanished between the hand-off and this loop (or never
        // carried a submitted message): there is nothing to watch. The spawn
        // inherited the guard, so hand it back — a stale hold would leave the
        // session busy forever.
        super::release_inflight(&handles, &facts.session_id).await;
        return;
    };
    let FollowFacts {
        session_id,
        thread_key: _,
        directory,
        started_at,
        anchor: _,
    } = facts;
    let flow = handles.flow();
    let disposition = settle::run(&flow, &session_id, &directory, timing, &owns).await;
    super::release_inflight(&handles, &session_id).await;
    let Some(disposition) = disposition else {
        return;
    };
    // The ownership re-check and the stamp share ONE cards lock (#539): the
    // guard is already released, so a new Turn may have replaced the card —
    // and its live card must never inherit this loop's ending. False means the
    // card is gone or replaced: stamp nothing and stay silent.
    if !owns.apply_if_held(&flow.cards, &session_id, &disposition).await {
        return;
    }
    super::send_completion_notice(
        &handles.cards,
        &handles.platform,
        &handles.config.notice_rules(),
        &session_id,
        started_at,
        &disposition,
    )
    .await;
}
