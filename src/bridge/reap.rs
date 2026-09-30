//! Session Sync's durable Live Card reap (ADR-0063, #438): every tick, each
//! record in [`LiveCards`](crate::bridge::live_cards::LiveCards) is reconciled
//! against the Session's own reads, so a card a cola restart orphaned stops
//! looking live.
//!
//! The pass reaps *state*, never content: a readable transcript's real ending
//! settles the card in place (✅ / ❌ / ⏳ 等待后台任务), an idle Session whose
//! Turn message never landed ends it 「⚠️ 这条消息未被接收」 — never ✅ — and a
//! still-live Session keeps the record. A card a successor took over is
//! collected as 「⏳ 已由新卡片接管 · 已停止更新」 at the takeover itself
//! ([`collect_orphan`], called by the card paths that arm over an orphan), so
//! two cards never both look live. Work that landed while cola was down is
//! published by the existing continuation machinery (a Wake's continuation
//! card) or simply left in the transcript: the reap never replays a turn onto
//! a stale card.
//!
//! Every action leaves one INFO line carrying the session and the decision —
//! never chat content. A read the reap could not make, or could not interpret,
//! claims nothing: the record stays for the next tick. That covers an
//! unrecognised status kind (unknown is not idle) and a record with no
//! directory to route the reads by (a cwd-routed read could be another
//! instance's run on V1). Neither PATCH takes a session's card-write lock: no
//! in-memory accumulator owns the card it targets (that is the reap's
//! precondition) and the ending it writes is a constant, so there is no
//! read-send-record sequence to serialize.

use crate::backend::TurnSettle;
use crate::bridge::handles::{CardsHandle, FlowHandles};
use crate::bridge::live_cards::LiveCard;
use crate::bridge::turn::Turn;
use crate::feishu::card::{CardState, error_line, shell::CardBuilder};

/// Collect the orphaned card `card_message_id` because a new card took the
/// chain over (ADR-0063): one PATCH naming the successor, terminal and grey.
/// Best-effort — a failed PATCH only warns; the record follows the successor
/// either way, so the freeze it leaves behind is the pre-#438 behavior, never
/// a crash.
pub(crate) async fn collect_orphan(cards: &CardsHandle, session_id: &str, card_message_id: &str) {
    if card_message_id.is_empty() {
        return;
    }
    let card = ending_card(CardState::TakenOver, None);
    match cards.feishu.update_message(card_message_id, &card).await {
        Ok(()) => tracing::info!("live-card reap: session {session_id} collected the orphaned card"),
        Err(e) => tracing::warn!(
            "live-card reap: session {session_id} could not collect card {card_message_id}: {e}"
        ),
    }
}

/// Reconcile one record against the Session's own reads (ADR-0063). `directory`
/// is the Session's mapped directory, when it still has one — the fallback
/// route when the record itself carries none — and `record` is the snapshot
/// the caller took from the sidecar. `read_timeout_ms` bounds each of the two
/// reads, the Session Sync pass's own request bound (injectable in tests), so
/// a hung server degrades to "nothing claimed" instead of freezing the tick.
pub(crate) async fn reconcile(
    handles: &FlowHandles,
    session_id: &str,
    directory: Option<&str>,
    record: &LiveCard,
    read_timeout_ms: u64,
) {
    // A live Turn (or the follow that inherited its guard) owns the session,
    // and an inbound message is about to: either way the card is not orphaned.
    if handles.waits.inflight.lock().await.contains(session_id)
        || handles.waits.inbound_pending(session_id).await
    {
        return;
    }
    // What does THIS process know about the session's card?
    let current_id = Turn::card_message_id(&handles.cards, session_id).await;
    // A live or yielded card is still running; a terminal one (and a missing
    // card) is not.
    let current_running = Turn::is_running(&handles.cards, session_id).await;
    if let Some(current_id) = current_id {
        if current_id == record.card_message_id {
            // The recorded card IS this process's card: its own lifecycle owns
            // it. A terminal card's record is spent (a write that raced the
            // terminal, or an ending stamped without the removal hook); a live
            // or yielded card keeps it.
            if !current_running {
                handles.cards.live_cards.remove(session_id);
            }
            return;
        }
        // A successor card owns the session while the record still names
        // another card — the record write raced the handover, or a path
        // attached the new id without tracking. Collect the recorded card in
        // place (ADR-0063's goal: never leave a card looking live), then name
        // the live successor; a settled successor keeps no record. The collect
        // is best-effort and harmless when the handover already collected it.
        collect_orphan(&handles.cards, session_id, &record.card_message_id).await;
        match (
            current_running,
            Turn::armed_turn_anchor(&handles.cards, session_id).await,
        ) {
            (true, Some(anchor)) => {
                handles.cards.live_cards.replace(
                    session_id,
                    LiveCard::new(current_id, anchor.message_id.clone(), Some(anchor.created_ms))
                        // The route belongs to the session, not the card: keep
                        // it across the re-point.
                        .with_directory(record.directory.clone()),
                );
            }
            // A settled successor has nothing to track, and an anchorless one
            // cannot scope a reap: the next track writes it when it can.
            _ => handles.cards.live_cards.remove(session_id),
        }
        return;
    }
    // No card in this process owns the session: the restart orphan (or a chain
    // that vanished). The Session's own reads decide, and a read the reap
    // could not make claims nothing.
    //
    // The reads route by the directory the record carried from track time; the
    // mapping's is the fallback. With neither, the reap decides nothing: on a
    // generation whose reads are per-directory instance (V1) a cwd-routed
    // status could belong to a different instance's run, and stamping over a
    // live run is worse than leaving a record for a later life (growth is
    // bounded by the live-card sessions).
    let directory = record
        .directory
        .as_deref()
        .filter(|directory| !directory.is_empty())
        .or(directory);
    let Some(directory) = directory else {
        tracing::debug!(
            "live-card reap: session {session_id} has no directory to route its reads; keeping the record"
        );
        return;
    };
    let status = match crate::bridge::bounded_call(
        "live-card reap status",
        read_timeout_ms,
        handles.backend.session_status(session_id, Some(directory)),
    )
    .await
    {
        Some(Ok(status)) => status,
        Some(Err(e)) => {
            tracing::warn!("live-card reap: session {session_id} status read failed: {e}");
            return;
        }
        None => return,
    };
    // Only a definite non-live status decides an ending. An unrecognised
    // status kind (`Ok(None)`) is unknown, not idle: `never guessed` — the
    // record stays for the next tick.
    let Some(status) = status else {
        return;
    };
    // A live Session keeps the card: the run may still answer it.
    if status.is_live() {
        return;
    }
    let transcript = match crate::bridge::bounded_call(
        "live-card reap transcript",
        read_timeout_ms,
        handles.backend.transcript(session_id),
    )
    .await
    {
        Some(Ok(transcript)) => transcript,
        Some(Err(e)) => {
            tracing::warn!("live-card reap: session {session_id} transcript read failed: {e}");
            return;
        }
        None => return,
    };
    // The settle decision's scope: the recorded anchor, else the submitted
    // message's own anchor re-derived from this read (it may have landed after
    // the record's last write). Absent, the message never landed — the
    // anchorless Unreceived scope.
    let scope = record
        .anchor()
        .or_else(|| transcript.anchor_of_user(record.message_id.as_str()));
    match transcript.settle(scope.as_ref()) {
        // The read's boundary rule is unsatisfied (a Wake's Execution has not
        // closed): the ending is not decided — keep observing.
        TurnSettle::Running => {}
        TurnSettle::Complete => {
            settle(handles, session_id, record, CardState::Done, None).await;
        }
        TurnSettle::Failed(error) => {
            settle(
                handles,
                session_id,
                record,
                CardState::Error,
                Some(&error_line(&error)),
            )
            .await;
        }
        // The wait is still on: the card yields 「⏳ 等待后台任务」 and keeps its
        // record, so a later read (this pass, every tick) settles the true end.
        // The ending is PATCHed once per life; a restart re-stamps it once.
        TurnSettle::Waiting => {
            if record.waiting_reaped {
                return;
            }
            if settle(handles, session_id, record, CardState::Waiting, None).await {
                handles
                    .cards
                    .live_cards
                    .mark_waiting_reaped(session_id, &record.card_message_id);
            }
        }
        // The submitted message never reached the transcript and the Session
        // is idle: nobody will answer it — never ✅ (ADR-0062).
        TurnSettle::Unreceived => {
            settle(handles, session_id, record, CardState::Unreceived, None).await;
        }
    }
}

/// PATCH `record`'s card into `state` and, when the state is terminal, drop the
/// record: nothing is owed a reap any more. Returns whether the PATCH landed
/// (a failed one keeps the record for the next tick). One INFO line per
/// action, naming the session and the decision — never chat content.
async fn settle(
    handles: &FlowHandles,
    session_id: &str,
    record: &LiveCard,
    state: CardState,
    detail: Option<&str>,
) -> bool {
    let card = ending_card(state.clone(), detail);
    if let Err(e) = handles
        .cards
        .feishu
        .update_message(&record.card_message_id, &card)
        .await
    {
        tracing::warn!(
            "live-card reap: session {session_id} could not settle card {}: {e}",
            record.card_message_id
        );
        return false;
    }
    tracing::info!("live-card reap: session {session_id} {}", state.reap_word());
    if state.is_terminal() {
        handles.cards.live_cards.remove(session_id);
    }
    true
}

/// A bare card carrying one ending — the reap's whole card vocabulary. The
/// lost turn's content is deliberately NOT rebuilt (ADR-0063): the ending's
/// header, the failure's own message when there is one, and no action. The
/// whole card is replaced, so whatever the orphan last showed — a half-drawn
/// turn, a stale spinner, a live tool panel — goes with it: the reap writes
/// state, never content. The Unreceived ending is therefore actionless here —
/// the restart took the accumulator its 重新发起 click would claim (the
/// original prompt and the card session), so the button could only be dead;
/// the user re-sends instead (ADR-0062's amendment).
fn ending_card(state: CardState, detail: Option<&str>) -> serde_json::Value {
    let mut builder = CardBuilder::new().with_state(state);
    if let Some(detail) = detail.filter(|detail| !detail.is_empty()) {
        builder = builder.with_text(detail);
    }
    builder.build()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reap's card vocabulary is one state + one optional line: the
    /// ending's header, the failure message when given, and no recovery
    /// button (the restart took the accumulator that could claim one).
    #[test]
    fn ending_cards_carry_the_state_and_no_action() {
        let done = ending_card(CardState::Done, None);
        assert_eq!(done["header"]["title"]["content"], "✅ 完成");
        assert!(done["body"]["elements"].as_array().unwrap().is_empty());

        let failed = ending_card(CardState::Error, Some("**错误**: 503"));
        assert_eq!(failed["header"]["title"]["content"], "❌ 出错");
        assert!(
            failed.to_string().contains("503"),
            "the failure's own message rides the card: {failed}"
        );

        let unreceived = ending_card(CardState::Unreceived, None);
        assert_eq!(unreceived["header"]["title"]["content"], "⚠️ 这条消息未被接收");
        assert!(
            unreceived["body"]["elements"]
                .as_array()
                .unwrap()
                .iter()
                .all(|element| element["tag"] != "button"),
            "the reap offers no recovery action: {unreceived}"
        );

        let waiting = ending_card(CardState::Waiting, None);
        assert_eq!(waiting["header"]["title"]["content"], "⏳ 等待后台任务");
    }
}
