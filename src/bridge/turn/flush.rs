//! The Turn's card flush/split state machine (spec #298, A2a).
//!
//! The operations sibling flows invoke live on `Turn` (`Turn::flush_card` /
//! `Turn::split_card_chain`) in the parent module; this module owns how a card
//! is built, sent, advanced and degraded, and nothing here is reachable from
//! outside the Turn module. The card-delivery helpers other flows genuinely use
//! (target resolution, stale marking) stay in their owning modules.

use std::sync::Arc;

use super::{MAX_CARD_CHAIN, PredecessorCollect, Turn};

use crate::bridge::card_handles::RenderedBlock;
use crate::bridge::chain::{RenderedCursor, release_spent};
use crate::bridge::handles::CardsHandle;

/// A one-shot test gate inside the staged-cursor drain reconcile (spec #561,
/// review #569): awaited between the delivery check and the confirmation, so a
/// test can stage a newer cursor in that exact window. Only tests construct
/// one; production passes `None`.
pub(crate) struct ReconcileGate {
    /// Signalled when the reconcile parked at the gate.
    pub(crate) entered: Arc<tokio::sync::Notify>,
    /// The reconcile proceeds once this is signalled.
    pub(crate) release: Arc<tokio::sync::Notify>,
}

/// Whether `e` is Feishu's deterministic card-content rejection (`230099`).
/// The same card JSON fails on every retry, so the flush degrades instead.
fn is_card_content_rejected(e: &crate::error::BridgeError) -> bool {
    matches!(e, crate::error::BridgeError::CardContentRejected { .. })
}

/// The card-handle registry record of `span`'s block, or `None` for a resolved
/// block's tombstone — nothing live to repaint. The block is the accumulator's
/// own render source, so the registry facts (kind, owner, receipt target) are
/// read off it at the one render seam.
fn rendered_block(
    span: &crate::bridge::card_handles::BlockSpan,
    block: &super::state::InteractionBlock,
) -> Option<RenderedBlock> {
    use super::state::InteractionBlock;
    use crate::bridge::snapshot_claims::ClaimKind;
    let kind = match block {
        InteractionBlock::Permission(_) => ClaimKind::Permission,
        InteractionBlock::Question(_) => ClaimKind::Question,
        InteractionBlock::Receipt(_) => return None,
    };
    Some(RenderedBlock {
        request_id: span.request_id.clone(),
        start: span.start,
        end: span.end,
        kind,
        session_id: block.session_id().to_string(),
        directory: block.directory().to_string(),
        target: block.receipt_target(),
    })
}

/// What a rejected card's flush should do next.
enum FallbackAdvance {
    /// Retry the same slice once, with model markdown fenced.
    RetryFenced,
    /// Stop: the fenced content was rejected too, so the card is suspended
    /// rather than PATCHed again on every poll.
    Stop,
}

/// Whether a flush may finalize the tracked card and continue on a new one.
#[derive(Clone, Copy)]
pub(super) enum SplitPolicy {
    /// The standard flush: a slice that outgrows its card finalizes and hands
    /// off to a continuation.
    Allow,
    /// A yielded card's ledger-only refresh (ADR-0060): the freeze's carve-out
    /// must never post a new card for a ledger update. The whole live slice —
    /// tail included — is re-rendered on the same card instead, with
    /// `render_from` untouched, so the chain keeps its card and the next flush
    /// resumes from the same place. The caller passes this only for a card
    /// that is still live with no split queued
    /// ([`Turn::refresh_yielded_ledger`](super::Turn::refresh_yielded_ledger)),
    /// so the unsplit build always takes the plain-update path. The ledger
    /// delta sits inside the splitter's own reserve margin, so the card stays
    /// under Feishu's hard cap even when its estimate crosses the split
    /// budget.
    Forbid,
}

/// Advance the turn's [`CardFallback`] after Feishu rejected a card it built.
async fn advance_card_fallback(cards: &CardsHandle, session_id: &str) -> FallbackAdvance {
    use crate::bridge::turn::state::CardFallback;
    let mut live = cards.cards.lock().await;
    let Some(card) = live.get_mut(session_id) else {
        return FallbackAdvance::Stop;
    };
    match card.acc.card_fallback {
        CardFallback::None => {
            card.acc.card_fallback = CardFallback::Fenced;
            FallbackAdvance::RetryFenced
        }
        CardFallback::Fenced | CardFallback::Suspended => {
            card.acc.card_fallback = CardFallback::Suspended;
            FallbackAdvance::Stop
        }
    }
}

/// Stage the Rendered Cursor of the body about to be written (spec #561):
/// `card_message_id` is `None` for a create, whose id is only known once the
/// send lands. Only a confirmed write drains the staged value. Returns the
/// stage identity the confirmation must name (spec #561, review #569); id `0`
/// means nothing was staged (no session), which no confirmation ever matches.
async fn stage_rendered_cursor(
    cards: &CardsHandle,
    session_id: &str,
    card_message_id: Option<&str>,
    cursor: &RenderedCursor,
) -> crate::bridge::turn::state::StagedCursorId {
    let mut live = cards.cards.lock().await;
    match live.get_mut(session_id) {
        Some(card) => {
            let id = card.acc.stage_cursor(card_message_id, cursor.clone());
            crate::bridge::turn::state::StagedCursorId {
                id,
                awaiting_seq: None,
            }
        }
        None => crate::bridge::turn::state::StagedCursorId {
            id: 0,
            awaiting_seq: None,
        },
    }
}

/// Confirm the staged Rendered Cursor once the write carrying it landed
/// (spec #561): the staged value is taken exactly once, mirrored into the
/// accumulator's confirmed base and advanced on the Chain Record, scoped to
/// the card the flush wrote. `chain_id` narrows the call to one chain when the
/// caller knows it (review #569): a fresh Turn that replaced the session
/// meanwhile owns its staged cursor itself, so its accumulator is never read,
/// mutated or advanced by a foreign confirmation. `expected` is the exact
/// stage the caller's write carried (its generation, plus the outbox sequence
/// a drain verified): a body staged SINCE that write — for example a fresh
/// flush whose PATCH is still pending — never matches and is left untouched
/// for its own confirmation (review #569).
async fn confirm_staged_cursor(
    cards: &CardsHandle,
    session_id: &str,
    card_message_id: &str,
    chain_id: Option<u64>,
    expected: crate::bridge::turn::state::StagedCursorId,
    gate: Option<&ConfirmGate>,
) {
    // ONE critical section (spec #561, review #569): the stage comparison —
    // `take_staged_cursor`'s last-applied guard — and the durable record write
    // happen atomically, so a concurrent confirmation can never interleave
    // between them. An older stage arriving after a newer one was applied is
    // discarded by the guard, and the durable cursor only ever moves forward.
    let mut live = cards.cards.lock().await;
    let Some(card) = live.get_mut(session_id) else {
        return;
    };
    if chain_id.is_some_and(|chain_id| card.chain_id() != chain_id) {
        return;
    }
    let Some(cursor) = card.acc.take_staged_cursor(card_message_id, expected) else {
        return;
    };
    // The ordering test parks one confirmation here (spec #561, review #569):
    // inside the critical section, so a newer confirmation waits for it.
    if let Some(gate) = gate {
        gate.entered.notify_one();
        gate.release.notified().await;
    }
    cards.chains.advance_cursor(session_id, card_message_id, &cursor);
}

/// A one-shot test gate inside one cursor confirmation (spec #561, review
/// #569): awaited after the stage take and before the durable write — inside
/// the confirmation's cards critical section — so the ordering test can
/// interleave a newer confirmation there. Only tests construct one; production
/// passes `None`.
pub(crate) struct ConfirmGate {
    /// Signalled when the confirmation parked at the gate.
    pub(crate) entered: Arc<tokio::sync::Notify>,
    /// The confirmation proceeds once this is signalled.
    pub(crate) release: Arc<tokio::sync::Notify>,
}

/// The test seam's confirmation entry (spec #561, review #569): confirm one
/// exact staged cursor, optionally parked at `gate`.
#[cfg(test)]
pub(crate) async fn confirm_staged_cursor_for_test(
    cards: &CardsHandle,
    session_id: &str,
    card_message_id: &str,
    expected: crate::bridge::turn::state::StagedCursorId,
    gate: Option<&ConfirmGate>,
) {
    confirm_staged_cursor(cards, session_id, card_message_id, None, expected, gate).await;
}

/// Fold a failed card write into the staged Rendered Cursor (spec #561): a
/// recoverable failure ties the stage to the Pending Card Update payload it
/// left owed — the drain reconcile confirms the cursor once that payload
/// delivers — while anything else (a permanent refusal, a create that will
/// never be retried) drops the stage, because no later write can carry it.
///
/// A recoverable failure whose payload the drain ALREADY delivered before this
/// note ran (the sequence lookup finds the payload settled, not owed) is a
/// confirmation, not a discard: the write reached the card, so the stage
/// advances now — a crash before another confirmed flush must never leave the
/// durable cursor behind content already on the card (spec #561, review #569).
async fn note_cursor_write_failure(
    cards: &CardsHandle,
    session_id: &str,
    card_message_id: Option<&str>,
    card: &serde_json::Value,
    error: &crate::error::BridgeError,
    stage: crate::bridge::turn::state::StagedCursorId,
) {
    if error.is_recoverable_card_write()
        && let Some(card_message_id) = card_message_id
    {
        if let Some(seq) = cards.feishu.pending_card_write(card_message_id, card) {
            // Still owed: the drain reconcile confirms the cursor once that
            // payload delivers.
            let mut live = cards.cards.lock().await;
            if let Some(session) = live.get_mut(session_id) {
                session.acc.await_cursor_write(stage, card_message_id, seq);
            }
            return;
        }
        // The payload is no longer owed: only ITS OWN sequence's delivered
        // verdict confirms the stage (spec #561, review #569). A newer write's
        // success — a cached repaint that may omit this body's delta — must
        // never advance the cursor, so the sequence asked about is the one
        // THIS payload failed at, not the entry's newest.
        if let Some(seq) = cards.feishu.failed_card_write(card_message_id, card)
            && cards.feishu.card_write_delivered(card_message_id, seq)
        {
            // The drain delivered this payload first: the write landed, so
            // confirm the exact stage this body carried.
            confirm_staged_cursor(cards, session_id, card_message_id, None, stage, None).await;
            return;
        }
    }
    let mut live = cards.cards.lock().await;
    if let Some(session) = live.get_mut(session_id) {
        session.acc.discard_staged_cursor(stage);
    }
}

/// Advance every staged Rendered Cursor whose owed Pending Card Update has
/// since delivered (spec #561). Runs after a drain: the failed payload a
/// cursor was tied to landed out of band, so the confirmation comes from the
/// delivery state rather than from a flush. Every retained stage is resolved
/// by its OWN sequence (review #569) — a delivered one persists even while a
/// newer stage holds the pending slot; one whose entry settled without
/// delivering it (a refusal, or a superseded payload) is dropped; a still-owed
/// one stays for its own confirmation.
pub(crate) async fn reconcile_staged_cursors(cards: &CardsHandle) {
    reconcile_staged(cards, None).await;
}

/// [`reconcile_staged_cursors`] with a test gate (spec #561, review #569):
/// the reconcile parks between the delivery check and the confirmation, so a
/// test can stage a newer cursor in that exact window.
#[cfg(test)]
pub(crate) async fn reconcile_staged_cursors_gated(cards: &CardsHandle, gate: &ReconcileGate) {
    reconcile_staged(cards, Some(gate)).await;
}

async fn reconcile_staged(cards: &CardsHandle, gate: Option<&ReconcileGate>) {
    let due: Vec<(String, String, u64, crate::bridge::turn::state::StagedCursorId)> = {
        let live = cards.cards.lock().await;
        live.iter()
            .flat_map(|(session_id, session)| {
                session
                    .acc
                    .staged_cursor_due()
                    .into_iter()
                    .map(|(card_message_id, seq, expected)| {
                        (session_id.clone(), card_message_id, seq, expected)
                    })
            })
            .collect()
    };
    for (session_id, card_message_id, seq, expected) in due {
        if cards.feishu.card_write_delivered(&card_message_id, seq) {
            if let Some(gate) = gate {
                gate.entered.notify_one();
                gate.release.notified().await;
            }
            // Only the stage whose delivery was just verified is confirmed: a
            // body staged since (a fresh flush whose PATCH is still pending) is
            // left for its own confirmation (review #569).
            confirm_staged_cursor(cards, &session_id, &card_message_id, None, expected, None).await;
        } else if cards
            .feishu
            .settled_card_write_delivered(&card_message_id)
            .is_some()
        {
            // The card's newest write settled at this or a newer sequence
            // without delivering this stage's payload (a permanent refusal, or
            // a payload a newer write superseded): nothing may advance by it,
            // and no later confirmation waits for it — drop the stage.
            discard_staged_cursor(cards, &session_id, expected).await;
        }
    }
}

/// Drop one staged cursor (spec #561, review #569) — the reconcile's own
/// cleanup for a stage whose payload can no longer deliver.
async fn discard_staged_cursor(
    cards: &CardsHandle,
    session_id: &str,
    expected: crate::bridge::turn::state::StagedCursorId,
) {
    let mut live = cards.cards.lock().await;
    if let Some(session) = live.get_mut(session_id) {
        session.acc.discard_staged_cursor(expected);
    }
}

/// The flush machine proper, entered with the session's card-write lock
/// already held by [`Turn::flush_card`](super::Turn::flush_card),
/// [`Turn::split_card_chain`](super::Turn::split_card_chain),
/// [`Turn::split_chain_for_wake`](super::Turn::split_chain_for_wake) or
/// [`Turn::refresh_yielded_ledger`](super::Turn::refresh_yielded_ledger).
/// `split_policy` decides whether an over-budget slice may finalize the card
/// and continue on a new one ([`SplitPolicy`]).
pub(super) async fn flush_card_locked(cards: &CardsHandle, session_id: &str, split_policy: SplitPolicy) {
    // A terminal card's durable record is dropped only once its ending write
    // is confirmed (ADR-0063 amendment): each ending PATCH below goes through
    // `chain::release_spent`, which re-checks the pending outbox before
    // releasing, so a failed final write keeps the record the reap needs.
    // The card-chain state a flush resumes from: a pending Supplement split
    // (ADR-0043) and whether the tracked card is still the live (growing) one.
    // Both survive the flush — a chain that exhausted the size bound, or died
    // between a finalize and its continuation, must be picked up where it
    // stopped, never restarted by overwriting the finalized slice.
    let (pending_split, mut card_is_live, suspended) = {
        let cards = cards.cards.lock().await;
        let Some(card) = cards.get(session_id) else {
            return;
        };
        (
            card.pending_split.clone(),
            card.card_is_live,
            card.acc.card_fallback == crate::bridge::turn::state::CardFallback::Suspended,
        )
    };
    if suspended {
        // The fenced fallback was rejected too: the card cannot be delivered,
        // and re-PATCHing it on every poll would only hammer the API.
        return;
    }
    // The size bound must never refuse a supplement split: it gets one
    // continuation slot of its own beyond the cap, and any remaining slice is
    // reconciled on the next flush.
    let flush_limit = if !pending_split.is_empty() {
        MAX_CARD_CHAIN + 1
    } else {
        MAX_CARD_CHAIN
    };
    // A chain that already owes a continuation (a failed send, or the remainder
    // of a bound-exhausted chain) will build it below: apply the receipts of
    // any supplements queued SINCE that attempt first, so they ride it. The
    // finalizing flush applies its receipts after the previous card's final
    // build instead (see the live branch), keeping them off that card.
    if !card_is_live {
        push_queued_receipts(cards, session_id).await;
    }
    for _ in 0..flush_limit {
        // The tracked card id, read BEFORE the build: while the loading card's
        // reply is still in flight the session has a live accumulator but no
        // card to send to, and building would advance `render_from` past a
        // slice nothing rendered. Leaving the state untouched lets the first
        // flush after the id lands serve it — including a Supplement split
        // requested in that window.
        let card_id = {
            let cards = cards.cards.lock().await;
            cards.get(session_id).and_then(|c| c.card_message_id.clone())
        };
        let Some(card_id) = card_id else { return };

        let (built, rendered, slice_from, slice_to) = {
            let mut cards = cards.cards.lock().await;
            let Some(card) = cards.get_mut(session_id) else {
                return;
            };
            // `build_card_with_info` advances `render_from` when the built
            // card is a finalized (full) slice. Remember the boundary: a
            // failed continuation send must restore it, or the slice it built
            // reaches no card and the retry silently starts after it.
            let slice_from = card.acc.render_from;
            let built = match split_policy {
                SplitPolicy::Allow => card.acc.build_card_with_info(),
                // A ledger-only refresh never finalizes: the whole live slice
                // renders on the tracked card, `render_from` unmoved.
                SplitPolicy::Forbid => card.acc.build_card_unsplit(),
            };
            let slice_to = card.acc.render_from;
            let rendered = built
                .spans
                .iter()
                .filter_map(|span| {
                    let block = card.acc.interaction(&span.request_id)?;
                    rendered_block(span, block)
                })
                .collect();
            (built, rendered, slice_from, slice_to)
        };

        if card_is_live {
            let supplement_split_requested = !pending_split.is_empty();
            if !supplement_split_requested && !built.full {
                // The live card still fits: a plain update. Stage this body's
                // Rendered Cursor first; only a confirmed write drains it. The
                // stage identity (and the watermark it can carry) is captured
                // before the write, so a body staged meanwhile is never
                // confirmed by this one (review #569).
                let stage = stage_rendered_cursor(cards, session_id, Some(&card_id), &built.cursor).await;
                let watermark = staged_watermark_id(cards, session_id).await;
                let delivered = match cards.feishu.update_message(&card_id, &built.card).await {
                    Ok(()) => true,
                    Err(e) => {
                        tracing::warn!("Card update failed: {}", e);
                        if is_card_content_rejected(&e) {
                            match advance_card_fallback(cards, session_id).await {
                                // Feishu refused the content: rebuild the SAME
                                // slice with every markdown element fenced and
                                // PATCH again. `render_from` did not advance, so
                                // the retry renders exactly this content.
                                FallbackAdvance::RetryFenced => continue,
                                // The fenced retry was rejected too — the card is
                                // suspended; stop instead of PATCHing forever.
                                FallbackAdvance::Stop => {
                                    // A suspended ending can never deliver, so a
                                    // terminal record is spent (never retried).
                                    // The staged cursor is dropped with it: no
                                    // later write can carry this body.
                                    note_cursor_write_failure(
                                        cards,
                                        session_id,
                                        Some(&card_id),
                                        &built.card,
                                        &e,
                                        stage,
                                    )
                                    .await;
                                    release_spent(cards, session_id).await;
                                    return;
                                }
                            }
                        }
                        note_cursor_write_failure(cards, session_id, Some(&card_id), &built.card, &e, stage)
                            .await;
                        false
                    }
                };
                // The ending write is settled — delivered, or permanently
                // refused: a terminal record may go once nothing is pending.
                release_spent(cards, session_id).await;
                // Record what this card now renders: the live blocks.
                cards
                    .card_handles
                    .lock()
                    .await
                    .record(&card_id, &built.card, rendered);
                if delivered {
                    // The confirmed PATCH carries this body's Rendered Cursor
                    // (spec #561): advance the chain's record — only while the
                    // exact stage this write carried is still the staged one.
                    confirm_staged_cursor(cards, session_id, &card_id, None, stage, None).await;
                    // The PATCH carried this slice's completion entries: the
                    // staged Wake Watermark is now user-visible (ADR-0061).
                    drain_wake_watermark(cards, session_id, watermark).await;
                }
                return;
            }
            // The tracked card is finalized: it overflowed (size split) or a
            // supplement forced the split. Either way the finalized card
            // renders NO tail — its live Interaction Blocks migrate to the
            // continuation (ADR-0038), so the old card's controls are settled
            // rather than left dead.
            let continues_an_ended_card = pending_split
                .iter()
                .any(|split| split.kind.continues_an_ended_card());
            // Whether the outgoing card received a ledger handover (ADR-0060)
            // before this split: its remaining list left, a retiring Wake's
            // completion entry arrived. A terminal card that received one
            // still owes the handover PATCH.
            let handover = pending_split.iter().any(|split| split.handover);
            let (finalized, should_patch) = if supplement_split_requested && !built.full {
                // The live slice still fits, but a supplement forces the split
                // anyway: finalize the slice here and HAND IT OFF — the
                // continuation carries only the receipts queued below and the
                // content that arrives after them, exactly like a size split.
                let mut cards = cards.cards.lock().await;
                let Some(card) = cards.get_mut(session_id) else {
                    return;
                };
                // A cause that continues an ENDED card (a Wake) advances the
                // render boundary either way, but only a waiting or live card
                // takes the standard 「部分完成，继续中…」 header — a terminal
                // card keeps the ending it recorded. It is still PATCHed when
                // the ledger handover wrote its last facts; without one it
                // keeps the ending it already shows and is not touched. Every
                // other cause splits a live card and re-stamps
                // unconditionally: its slice may not have reached Feishu yet
                // (the loading-card window's deferred split).
                let terminal = continues_an_ended_card && card.acc.card_state.is_terminal();
                let state = terminal.then(|| card.acc.card_state.clone());
                let finalized = card.acc.build_finalized_handoff(state);
                (finalized, !terminal || handover)
            } else {
                (built, true)
            };
            // Persist the finalization BEFORE the send: a failed or cancelled
            // send must not leave the next flush thinking this card still
            // grows — it would overwrite the finalized slice.
            {
                let mut cards = cards.cards.lock().await;
                if let Some(card) = cards.get_mut(session_id) {
                    card.card_is_live = false;
                    if continues_an_ended_card {
                        // The continuation card is a fresh live card: the
                        // ended attempt's display facts stay with the card the
                        // handoff just finalized (ADR-0059).
                        card.acc.continue_on_new_card();
                    }
                }
            }
            card_is_live = false;
            if supplement_split_requested {
                // Record EVERY queued supplement with its own receipt line, in
                // arrival order: written AFTER the previous card's final build
                // and BEFORE the continuation's, so they ride the continuation
                // only. The per-entry flag keeps a retried flush from doubling
                // them.
                push_queued_receipts(cards, session_id).await;
            }
            // Stage the finalized body's Rendered Cursor before its PATCH; only
            // the confirmed write drains it — and only while this exact stage is
            // still the staged one (review #569). A slice that is not PATCHed
            // stages nothing.
            let stage = if should_patch {
                Some(stage_rendered_cursor(cards, session_id, Some(&card_id), &finalized.cursor).await)
            } else {
                None
            };
            let watermark = if should_patch {
                staged_watermark_id(cards, session_id).await
            } else {
                None
            };
            let delivered = if should_patch {
                let stage = stage.expect("a PATCHed slice stages its cursor");
                match cards.feishu.update_message(&card_id, &finalized.card).await {
                    Ok(()) => true,
                    Err(e) => {
                        tracing::warn!("Card update failed: {}", e);
                        if is_card_content_rejected(&e) {
                            if matches!(
                                advance_card_fallback(cards, session_id).await,
                                FallbackAdvance::RetryFenced
                            ) {
                                // The finalized slice never reached Feishu: restore it
                                // and re-send it fenced on the same card, instead of
                                // losing it to a rejection that would repeat verbatim.
                                let mut cards = cards.cards.lock().await;
                                if let Some(card) = cards.get_mut(session_id)
                                    && card.acc.render_from == slice_to
                                {
                                    card.acc.render_from = slice_from;
                                    card.card_is_live = true;
                                    card_is_live = true;
                                }
                                continue;
                            }
                            // The fenced retry was rejected too: the card is
                            // suspended, so stop the chain instead of building the
                            // next card out of content the platform may refuse too.
                            // A suspended ending can never deliver, so a terminal
                            // record is spent (never retried), and its staged
                            // cursor is dropped with it.
                            note_cursor_write_failure(
                                cards,
                                session_id,
                                Some(&card_id),
                                &finalized.card,
                                &e,
                                stage,
                            )
                            .await;
                            release_spent(cards, session_id).await;
                            cards
                                .card_handles
                                .lock()
                                .await
                                .record(&card_id, &finalized.card, Vec::new());
                            return;
                        }
                        note_cursor_write_failure(
                            cards,
                            session_id,
                            Some(&card_id),
                            &finalized.card,
                            &e,
                            stage,
                        )
                        .await;
                        false
                    }
                }
            } else {
                false
            };
            // The ending write is settled — delivered, or permanently refused:
            // a terminal record may go once nothing is pending.
            release_spent(cards, session_id).await;
            cards
                .card_handles
                .lock()
                .await
                .record(&card_id, &finalized.card, Vec::new());
            if delivered {
                // The confirmed PATCH carries this body's Rendered Cursor
                // (spec #561): advance the chain's record — only while the
                // exact stage this write carried is still the staged one.
                if let Some(stage) = stage {
                    confirm_staged_cursor(cards, session_id, &card_id, None, stage, None).await;
                }
                // The finalized PATCH delivered this slice's entries; with no
                // split queued the mark is fully user-visible, so it may drain
                // (a queued split's 承接 line still owes the continuation send
                // — the drain helper holds it back).
                drain_wake_watermark(cards, session_id, watermark).await;
            }
            continue;
        }

        // The tracked card is a FINALIZED slice: the chain continues on a NEW
        // card. Either split already advanced `render_from` past the finalized
        // content — a size split inside `build_card_with_info`, a supplement
        // split in `build_finalized_handoff` — so this card renders only the
        // delta after it (the receipts included), never the prior content
        // again.
        //
        // Where it goes: the newest queued split's message, else the chain's
        // own reply target, else — a top-level Wake continuation armed after a
        // restart, which has no user message to reply to — the Chat the card
        // was sent to. Only when neither is known does the chain stop: nothing
        // can reach a card that answers nowhere.
        let (reply_to, fallback_chat) = {
            let live = cards.cards.lock().await;
            let card = live.get(session_id);
            match pending_split.last() {
                // A supplement split anchors its chain at the NEWEST
                // supplement: one continuation serves the whole queued batch.
                Some(split) => (Some(split.reply_to.clone()), None),
                None => (
                    card.and_then(|c| c.acc.reply_to_message_id.clone()),
                    card.and_then(|c| c.fallback_chat.clone()),
                ),
            }
        };
        if reply_to.is_none() && fallback_chat.is_none() {
            // Nothing can reach a card: no continuation is attempted, so no
            // cursor is staged.
            return;
        }
        // Stage this continuation body's Rendered Cursor before the create:
        // creates are never outbox-retried, so only this send's own Ok drains
        // it (the new card's id is attached after `track_live_card`) — and only
        // while this exact stage is still the staged one (review #569).
        let stage = stage_rendered_cursor(cards, session_id, None, &built.cursor).await;
        let watermark = staged_watermark_id(cards, session_id).await;
        let sent = match (reply_to.as_deref(), fallback_chat.as_deref()) {
            (Some(reply_to), _) => cards.feishu.reply_card(reply_to, &built.card).await,
            (None, Some(chat)) => cards.feishu.send_card("chat_id", chat, &built.card).await,
            (None, None) => unreachable!("checked above"),
        };
        match sent {
            Ok(new_id) => {
                {
                    let mut cards = cards.cards.lock().await;
                    if let Some(card) = cards.get_mut(session_id) {
                        card.card_message_id = Some(new_id.clone());
                        // A continuation that fits becomes the live card; one
                        // that is itself over the budget stays finalized and
                        // loops for the next slice.
                        card.card_is_live = !built.full;
                        if !pending_split.is_empty() {
                            // The continuation is the chain's new anchor: a
                            // later size split (and the group completion
                            // notice) continues from the newest supplement. The
                            // queue is served — its receipts are on the card —
                            // so a later supplement starts a fresh one.
                            if let Some(newest) = pending_split.last() {
                                card.acc.reply_to_message_id = Some(newest.reply_to.clone());
                            }
                            card.pending_split.clear();
                        }
                    }
                }
                card_is_live = !built.full;
                // The chain continues on a new card: the durable record
                // follows it (ADR-0063). No predecessor is collected — the
                // split above finalized the outgoing card itself.
                Turn::track_live_card(cards, session_id, &new_id, PredecessorCollect::Never, None).await;
                // The continuation takes the blocks over from the finalized
                // slice it follows (its spans are the tail this card renders).
                cards
                    .card_handles
                    .lock()
                    .await
                    .record(&new_id, &built.card, rendered);
                // The confirmed create carries this body's Rendered Cursor
                // (spec #561): advance the chain's record, now naming the new
                // card — only while the exact stage this send carried is still
                // the staged one.
                confirm_staged_cursor(cards, session_id, &new_id, None, stage, None).await;
                // The send delivered the 承接 line (or the size-split slice):
                // the staged Wake Watermark is user-visible now (ADR-0061).
                drain_wake_watermark(cards, session_id, watermark).await;
                if !built.full {
                    return;
                }
                // The continuation is itself over the limit → loop for the
                // next slice's card.
            }
            Err(e) => {
                tracing::warn!("Card continuation send failed: {}", e);
                // The create reached no card and is never retried: its staged
                // cursor is dropped (a fenced retry rebuilds and re-stages).
                note_cursor_write_failure(cards, session_id, None, &built.card, &e, stage).await;
                let retry_fenced = is_card_content_rejected(&e)
                    && matches!(
                        advance_card_fallback(cards, session_id).await,
                        FallbackAdvance::RetryFenced
                    );
                // The failed send's slice reached no card, but its build
                // already advanced `render_from` past it: restore the
                // boundary so the retry re-renders the SAME slice instead of
                // silently skipping it. The equality check keeps a boundary
                // moved elsewhere (defensive; the card-write lock serializes
                // flushes) from being rewound.
                let mut cards = cards.cards.lock().await;
                if let Some(card) = cards.get_mut(session_id)
                    && card.acc.render_from == slice_to
                {
                    card.acc.render_from = slice_from;
                }
                drop(cards);
                if retry_fenced {
                    // Retry the same slice once, fenced; a second rejection
                    // falls through to the plain retry-next-flush path.
                    continue;
                }
                return;
            }
        }
    }
}

/// Write one receipt line per queued supplement that does not have one yet, in
/// arrival order. Exactly once per supplement: the per-entry flag travels with
/// the queue, so a flush retried after a failed continuation send re-applies
/// nothing — and a supplement queued after that attempt still gets its line.
async fn push_queued_receipts(cards: &CardsHandle, session_id: &str) {
    let mut cards = cards.cards.lock().await;
    let Some(card) = cards.get_mut(session_id) else {
        return;
    };
    for i in 0..card.pending_split.len() {
        if card.pending_split[i].receipt_pushed {
            continue;
        }
        let kind = card.pending_split[i].kind;
        let line = card.pending_split[i].line.clone();
        card.acc
            .push_receipt_at(line.as_ref().map(|line| line.at), kind.receipt());
        if let Some((wake, created_ms)) = line.as_ref().and_then(|line| line.wake.as_ref()) {
            // The 承接 line announces this Wake's completion and hands its work
            // to the continuation card: mark both, so the merged-path entry (a
            // Wake that resumes an already-live card) cannot double it, and a
            // later tail past the Wake still splits (ADR-0059) instead of
            // resuming the continuation in place. The announcement also stages
            // the durable Wake Watermark, which advances only when this line's
            // card actually sends (ADR-0061).
            card.acc.announce_wake(wake, *created_ms);
            card.acc.hand_over_wake(wake);
        }
        card.pending_split[i].receipt_pushed = true;
    }
}

/// The stage generation of the session's staged Wake Watermark, when one is
/// staged: what its drain must match (spec #561, review #569), so a watermark
/// staged after a write is never drained by it.
async fn staged_watermark_id(cards: &CardsHandle, session_id: &str) -> Option<u64> {
    cards
        .cards
        .lock()
        .await
        .get(session_id)
        .and_then(|card| card.acc.pending_watermark_id())
}

/// Persist the chain's staged Wake Watermark (ADR-0061) after a card write
/// delivered it. A queued split keeps the mark staged: its 承接 line still
/// owes its own send, and until that lands the covered Wake is not
/// user-visible, so a crash here must leave it unannounced for the next
/// restart. The staged value is cleared only on a successful drain. The
/// projections' confirmed creates drain through this same choke point (spec
/// #561, ticket #566), so a Wake their successor rendered cannot be
/// re-announced by a later recordless Fresh post. `expected` is the stage the
/// delivered write carried: a watermark staged since is left untouched
/// (review #569).
pub(crate) async fn drain_wake_watermark(cards: &CardsHandle, session_id: &str, expected: Option<u64>) {
    drain_staged_watermark(cards, session_id, None, expected).await;
}

/// [`drain_wake_watermark`] scoped to the armed successor's chain identity
/// (spec #561, review #569): after a projection's atomic takeover a fresh Turn
/// can still replace the session before this runs — its staged watermark is
/// its own chain's, so only the armed accumulator's is ever drained.
pub(crate) async fn drain_armed_watermark(
    cards: &CardsHandle,
    session_id: &str,
    chain_id: u64,
    expected: Option<u64>,
) {
    drain_staged_watermark(cards, session_id, Some(chain_id), expected).await;
}

/// The shared drain body: take the exact staged Wake Watermark a delivered
/// write carried and persist it — optionally only while the session is still
/// `chain_id`'s (spec #561, review #569).
async fn drain_staged_watermark(
    cards: &CardsHandle,
    session_id: &str,
    chain_id: Option<u64>,
    expected: Option<u64>,
) {
    let mut live = cards.cards.lock().await;
    let Some(card) = live.get_mut(session_id) else {
        return;
    };
    if chain_id.is_some_and(|chain_id| card.chain_id() != chain_id) {
        return;
    }
    if !card.pending_split.is_empty() {
        return;
    }
    // The delivered write's own stage drains even when a newer mark has
    // been staged since: a durable announcement is never lost to a later
    // stage (review #569). The take and the durable write share ONE critical
    // section, and `ChainRecords::advance` is monotonic by `created_ms`, so
    // even a stale drain could never move the mark backwards.
    let Some(staged) = expected.and_then(|expected| card.acc.take_staged_watermark(expected)) else {
        return;
    };
    cards
        .chains
        .advance(session_id, &staged.wake_id, staged.created_ms);
}

#[cfg(test)]
mod tests {
    //! Card-content rejection recovery (`230099`): Feishu refuses to compile a
    //! card whose markdown it cannot parse (a model-written Feishu tag), whose
    //! table budget is blown, or whose image key is invalid. The rejection is
    //! deterministic — resending the same JSON fails on every future flush, which
    //! silently freezes the turn's card. The flush must recognize the typed error,
    //! degrade every model-markdown element to a code fence (the one form the
    //! parser accepts unconditionally), and retry the same slice once.

    use std::sync::Arc;

    use super::{drain_wake_watermark, note_cursor_write_failure, staged_watermark_id};
    use crate::bridge::test_support::*;
    use crate::bridge::turn::Turn;
    use crate::bridge::turn::state::{CardFallback, CardSession, StreamAccumulator};
    use crate::feishu::card::CardState;

    /// The markdown content of the first body element.
    fn first_markdown(card: &serde_json::Value) -> String {
        card["body"]["elements"][0]["content"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }

    /// Every card the platform was asked to send, in call order.
    async fn sent_cards(platform: &RecordingPlatform) -> Vec<serde_json::Value> {
        platform
            .calls
            .lock()
            .await
            .iter()
            .filter_map(|c| match c {
                PlatformCall::ReplyCard { card, .. } => Some(card.clone()),
                _ => None,
            })
            .collect()
    }

    /// Seed a live card whose text holds a construct Feishu refuses (a bare
    /// Feishu tag) and the in-flight guard.
    async fn seed_live_card(app: &Arc<App>, text: &str) {
        let mut acc = StreamAccumulator::new("回合");
        acc.card_state = CardState::Streaming;
        acc.push_text(text);
        acc.reply_to_message_id = Some("msg_1".into());
        app.cards
            .lock()
            .await
            .insert("ses_test".into(), CardSession::new(acc, Some("om_live".into())));
        app.inflight.lock().await.insert("ses_test".to_string());
    }

    /// A test app with `ses_test` seeded, plus its recording platform.
    async fn app_with_session() -> (Arc<App>, Arc<RecordingPlatform>) {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
        seed_session(&app, "ses_test", "/work").await;
        (app, platform)
    }

    /// The same, with a live card carrying `text` and the in-flight guard.
    async fn app_with_live_card(text: &str) -> (Arc<App>, Arc<RecordingPlatform>) {
        let (app, platform) = app_with_session().await;
        seed_live_card(&app, text).await;
        (app, platform)
    }

    /// A `230099` rejection flips the turn to the fenced fallback and the SAME
    /// slice is re-sent fenced — the card recovers instead of freezing.
    #[tokio::test]
    async fn a_rejected_card_update_is_retried_fenced() {
        let (app, platform) = app_with_live_card("回答 <number_tag> 里。").await;
        platform
            .fail_update_card_content_count
            .store(1, std::sync::atomic::Ordering::SeqCst);

        Turn::flush_card(&app.cards_handle(), "ses_test").await;

        let updates = patches_to(&platform, "om_live").await;
        assert_eq!(
            updates.len(),
            2,
            "the rejected attempt and the fenced retry: {updates:?}"
        );
        assert!(
            first_markdown(&updates[0]).contains("&#60;number_tag>"),
            "the first attempt is the plain sanitized render: {}",
            updates[0]
        );
        let retry = first_markdown(&updates[1]);
        assert!(
            retry.starts_with("```") && retry.contains("<number_tag>"),
            "the retry fences the model markdown: {retry}"
        );
        let cards = app.cards.lock().await;
        assert_eq!(
            cards.get("ses_test").unwrap().acc.card_fallback,
            CardFallback::Fenced,
            "the fallback is sticky for the turn"
        );
    }

    /// A rejection on the FINALIZED patch (the live slice overflowed and was
    /// finalized in place) must restore the slice and re-send it fenced on the
    /// same card — not lose it to a continuation that starts after it.
    #[tokio::test]
    async fn a_rejected_finalized_update_is_re_sent_fenced_on_the_same_card() {
        let (app, platform) = app_with_session().await;

        // Two exactly-full text slices: the first build fills the card and
        // finalizes it (advancing `render_from` to the second slice).
        let max = crate::feishu::card::MAX_CARD_TEXT_CHARS;
        let slice = |i: usize| format!("【S{i:02}】{}", "长".repeat(max - 5));
        let mut acc = StreamAccumulator::new("回合");
        acc.card_state = CardState::Streaming;
        acc.push_text(&slice(0));
        acc.push_text(&slice(1));
        acc.reply_to_message_id = Some("msg_1".into());
        app.cards
            .lock()
            .await
            .insert("ses_test".into(), CardSession::new(acc, Some("om_live".into())));
        app.inflight.lock().await.insert("ses_test".to_string());
        platform
            .fail_update_card_content_count
            .store(1, std::sync::atomic::Ordering::SeqCst);

        Turn::flush_card(&app.cards_handle(), "ses_test").await;

        let updates = patches_to(&platform, "om_live").await;
        assert_eq!(
            updates.len(),
            2,
            "the rejected finalize and its fenced re-send: {updates:?}"
        );
        assert!(
            !first_markdown(&updates[0]).starts_with("```"),
            "the first finalize is the plain render"
        );
        assert!(
            first_markdown(&updates[1]).starts_with("```"),
            "the re-send fences the slice: {}",
            updates[1]
        );
        // The second slice continues on a new card, as a normal full-slice flush.
        let continuation = platform
            .calls
            .lock()
            .await
            .iter()
            .find_map(|c| match c {
                PlatformCall::ReplyCard { card, .. } => Some(card.clone()),
                _ => None,
            })
            .expect("the remainder continues on a new card");
        assert!(
            continuation.to_string().contains("【S01】"),
            "the continuation carries the second slice: {continuation}"
        );
    }

    /// The continuation `reply_card` can be rejected too: its retry re-renders the
    /// same slice fenced and lands.
    #[tokio::test]
    async fn a_rejected_continuation_send_is_retried_fenced() {
        let (app, platform) = app_with_live_card("回答 <number_tag> 里。").await;
        // The tracked card is already finalized, so this flush owes a continuation.
        {
            let mut cards = app.cards.lock().await;
            let session = cards.get_mut("ses_test").unwrap();
            session.card_is_live = false;
            session.acc.reply_to_message_id = Some("msg_1".into());
        }
        platform
            .fail_reply_card_content_count
            .store(1, std::sync::atomic::Ordering::SeqCst);

        Turn::flush_card(&app.cards_handle(), "ses_test").await;

        let cards = sent_cards(&platform).await;
        assert_eq!(
            cards.len(),
            2,
            "the rejected send and the fenced retry: {cards:?}"
        );
        assert!(
            cards[1].to_string().contains("```") && cards[1].to_string().contains("<number_tag>"),
            "the retry fences the model markdown: {}",
            cards[1]
        );
        assert_eq!(
            app.cards.lock().await.get("ses_test").unwrap().acc.card_fallback,
            CardFallback::Fenced
        );
    }

    /// A terminal card whose ending Feishu permanently refuses — rejected
    /// plain, then rejected fenced — can never deliver, so its durable record
    /// is spent there and then: nothing is owed a reap, and the suspended card
    /// is never PATCHed again. This is the suspended-ending half of ADR-0067's
    /// release rule (the recoverable-failure half keeps the record until the
    /// outbox drains).
    #[tokio::test]
    async fn a_suspended_terminal_ending_spends_the_record() {
        let (app, platform) = app_with_live_card("回答 <number_tag> 里。").await;
        {
            let mut cards = app.cards.lock().await;
            cards.get_mut("ses_test").unwrap().acc.card_state = CardState::Done;
        }
        app.cards_handle().chains.track(
            "ses_test",
            "om_live",
            crate::backend::MessageId::new("msg_1"),
            Some(1_000),
            Some("/work"),
        );
        platform
            .fail_update_card_content_count
            .store(2, std::sync::atomic::Ordering::SeqCst);

        Turn::flush_card(&app.cards_handle(), "ses_test").await;

        assert_eq!(
            patches_to(&platform, "om_live").await.len(),
            2,
            "the plain attempt and the fenced one only"
        );
        assert_eq!(
            app.cards.lock().await.get("ses_test").unwrap().acc.card_fallback,
            CardFallback::Suspended
        );
        assert!(
            app.cards_handle().chains.get("ses_test").is_none(),
            "a permanently refused ending spends the record"
        );
    }

    /// A second rejection means fencing cannot help: the card is suspended, and a
    /// later flush makes no further PATCH attempts (no API hammering).
    #[tokio::test]
    async fn a_second_rejection_suspends_the_card() {
        let (app, platform) = app_with_live_card("回答 <number_tag> 里。").await;
        platform
            .fail_update_card_content_count
            .store(2, std::sync::atomic::Ordering::SeqCst);

        Turn::flush_card(&app.cards_handle(), "ses_test").await;
        assert_eq!(
            patches_to(&platform, "om_live").await.len(),
            2,
            "the plain attempt and the fenced one only"
        );
        assert_eq!(
            app.cards.lock().await.get("ses_test").unwrap().acc.card_fallback,
            CardFallback::Suspended
        );

        // A later poll must not PATCH the suspended card again.
        Turn::flush_card(&app.cards_handle(), "ses_test").await;
        assert_eq!(
            patches_to(&platform, "om_live").await.len(),
            2,
            "a suspended card is not retried on later flushes"
        );
    }

    /// The Wake Watermark drain names the exact stage the delivered write
    /// carried (spec #561, review #569): a watermark staged after that write is
    /// left for its own confirmation, so a durable announcement never runs
    /// ahead of the card that shows it.
    #[tokio::test]
    async fn the_watermark_drain_names_the_delivered_write_stage() {
        let (app, _platform) = app_with_session().await;
        let cards = app.cards_handle();
        Turn::seed_card(&cards, "ses_test", Some("om_live")).await;

        // A read stages the Wake Watermark while the write is in flight.
        {
            let mut live = cards.cards.lock().await;
            let card = live.get_mut("ses_test").expect("the seeded card");
            assert!(card.acc.announce_wake("wake_1", 1_000));
        }
        let staged = staged_watermark_id(&cards, "ses_test")
            .await
            .expect("a staged watermark");

        // A confirmation for a write that carried no watermark — or an earlier
        // stage — must not drain it.
        drain_wake_watermark(&cards, "ses_test", None).await;
        assert_eq!(
            cards.chains.announced("ses_test"),
            None,
            "a write with no staged watermark drains nothing"
        );
        drain_wake_watermark(&cards, "ses_test", Some(staged.wrapping_add(1))).await;
        assert_eq!(
            cards.chains.announced("ses_test"),
            None,
            "a superseded stage is left for its own confirmation"
        );

        // The write that carried it drains it exactly once.
        drain_wake_watermark(&cards, "ses_test", Some(staged)).await;
        assert_eq!(
            cards.chains.announced("ses_test").map(|mark| mark.created_ms),
            Some(1_000),
            "the matching stage drains"
        );
        drain_wake_watermark(&cards, "ses_test", Some(staged)).await;
        assert_eq!(
            cards.chains.announced("ses_test").map(|mark| mark.created_ms),
            Some(1_000),
            "a drained stage never advances twice"
        );
    }

    /// A delivered stage still drains after a NEWER Wake Watermark is staged
    /// (spec #561, review #569): the durable announcement must not be lost
    /// when the mark advances while the delivered write's drain is in flight.
    #[tokio::test]
    async fn a_delivered_watermark_drains_though_a_newer_stage_replaced_it() {
        let (app, _platform) = app_with_session().await;
        let cards = app.cards_handle();
        Turn::seed_card(&cards, "ses_test", Some("om_live")).await;

        {
            let mut live = cards.cards.lock().await;
            let card = live.get_mut("ses_test").expect("the seeded card");
            assert!(card.acc.announce_wake("wake_1", 1_000));
        }
        let delivered = staged_watermark_id(&cards, "ses_test")
            .await
            .expect("a staged watermark");
        // A newer Wake stages while the delivered write's drain has not run.
        {
            let mut live = cards.cards.lock().await;
            let card = live.get_mut("ses_test").expect("the seeded card");
            assert!(card.acc.announce_wake("wake_2", 2_000));
        }

        // The delivered write's own stage drains: its mark is persisted even
        // though the newer stage now rides the accumulator.
        drain_wake_watermark(&cards, "ses_test", Some(delivered)).await;
        assert_eq!(
            cards.chains.announced("ses_test").map(|mark| mark.created_ms),
            Some(1_000),
            "the delivered stage persists despite the newer one"
        );
        let newer = staged_watermark_id(&cards, "ses_test")
            .await
            .expect("the newer stage stays staged for its own drain");
        drain_wake_watermark(&cards, "ses_test", Some(newer)).await;
        assert_eq!(
            cards.chains.announced("ses_test").map(|mark| mark.created_ms),
            Some(2_000),
            "the newer stage then advances the mark"
        );
    }

    /// A newer cached repaint succeeds before the failure note runs (spec
    /// #561, review #569): its success must never confirm the OLDER failed
    /// write's stage — the repaint's body may omit that write's delta, and a
    /// restart would then skip content that never reached a card.
    #[tokio::test]
    async fn a_newer_repaint_never_confirms_an_older_failed_stages_cursor() {
        use crate::backend::MessageId;
        use crate::bridge::chain::{CursorFrontier, CursorPartKind, RenderedCursor, cursor_prefix_digest};
        let (app, platform) = app_with_live_card("回答。").await;
        let cards = app.cards_handle();
        cards.chains.track(
            "ses_test",
            "om_live",
            MessageId::new("msg_cola_anchor"),
            Some(1_000),
            None,
        );
        let card = serde_json::json!({ "schema": "2.0" });
        // The write fails recoverably: the payload is owed...
        platform
            .fail_update_transport_count
            .store(1, std::sync::atomic::Ordering::SeqCst);
        let error = cards
            .feishu
            .update_message("om_live", &card)
            .await
            .expect_err("the transport failure");
        // ...and a NEWER cached repaint — a different, older body that may omit
        // this write's delta — succeeds before the failure note runs.
        let repaint = serde_json::json!({ "schema": "2.0", "header": { "title": "repaint" } });
        cards
            .feishu
            .update_message("om_live", &repaint)
            .await
            .expect("the repaint lands");
        assert_eq!(
            cards.feishu.settled_card_write_delivered("om_live"),
            Some(true),
            "the newest write settled delivered"
        );

        let cursor = RenderedCursor {
            frontier: Some(CursorFrontier {
                message_id: MessageId::new("msg_a_2000"),
                part_index: 0,
                kind: CursorPartKind::Text,
                started_at: Some(2_000),
                delivered_chars: "回答".chars().count(),
                prefix_digest: Some(cursor_prefix_digest("回答")),
            }),
            live_calls: Default::default(),
        };
        let stage = Turn::stage_cursor(&cards, "ses_test", Some("om_live"), &cursor).await;
        note_cursor_write_failure(&cards, "ses_test", Some("om_live"), &card, &error, stage).await;

        assert_eq!(
            cards.chains.cursor("ses_test"),
            None,
            "a newer repaint's success never confirms the older failed write's cursor"
        );
    }

    /// A failure note whose payload the drain delivered before the note looked
    /// up its sequence must CONFIRM the staged cursor, not discard it (spec
    /// #561, review #569): the write reached the card, so the durable cursor
    /// must cover it — a crash before another confirmed flush would otherwise
    /// repeat content the card already shows.
    #[tokio::test]
    async fn a_failure_note_after_the_drain_delivered_confirms_the_stage() {
        use crate::backend::MessageId;
        use crate::bridge::chain::{CursorFrontier, CursorPartKind, RenderedCursor, cursor_prefix_digest};
        let (app, platform) = app_with_live_card("回答。").await;
        let cards = app.cards_handle();
        cards.chains.track(
            "ses_test",
            "om_live",
            MessageId::new("msg_cola_anchor"),
            Some(1_000),
            None,
        );
        let card = serde_json::json!({ "schema": "2.0" });
        // The write fails recoverably: the payload is owed...
        platform
            .fail_update_transport_count
            .store(1, std::sync::atomic::Ordering::SeqCst);
        let error = cards
            .feishu
            .update_message("om_live", &card)
            .await
            .expect_err("the transport failure");
        assert!(
            cards.feishu.has_pending_card_update("om_live"),
            "a recoverable failure leaves the payload owed"
        );
        // ...and the drain delivers it before the failure note runs.
        cards.feishu.drain_pending_card_updates(true).await;
        assert!(!cards.feishu.has_pending_card_update("om_live"));
        assert_eq!(
            cards.feishu.settled_card_write_delivered("om_live"),
            Some(true),
            "the settled write delivered"
        );

        let cursor = RenderedCursor {
            frontier: Some(CursorFrontier {
                message_id: MessageId::new("msg_a_2000"),
                part_index: 0,
                kind: CursorPartKind::Text,
                started_at: Some(2_000),
                delivered_chars: "回答".chars().count(),
                prefix_digest: Some(cursor_prefix_digest("回答")),
            }),
            live_calls: Default::default(),
        };
        let stage = Turn::stage_cursor(&cards, "ses_test", Some("om_live"), &cursor).await;
        note_cursor_write_failure(&cards, "ses_test", Some("om_live"), &card, &error, stage).await;

        assert_eq!(
            cards.chains.cursor("ses_test"),
            Some(cursor),
            "the drain-delivered write advances the staged cursor"
        );
        assert_eq!(
            Turn::staged_cursor(&cards, "ses_test").await,
            None,
            "the stage was consumed exactly once"
        );
    }
}
