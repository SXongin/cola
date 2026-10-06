//! The out-of-turn settle loop the follow, the unreceived watch and the Wake
//! continuation share (ADR-0059, ADR-0062).
//!
//! Each loop watches a card no live Turn owns: they poll the Session's
//! transcript and status on an injected cadence, stream into the card, and end
//! through the single settle decision — never from a terminal step — with no
//! total budget. The only ceilings are the two graces for states nobody can
//! act on, exactly as #386 defined them: **lost contact** (no tick in the
//! grace produced a full read pair) and **an unreconcilable live panel** (a
//! readable, settled card still carries a `⏳` panel). A `/stop` ends either
//! loop promptly in the stop terminal (#394), after one last reconcile render.
//!
//! The loops differ in exactly two facts, both parameters here:
//!
//! - **what owns the card** ([`Ownership`]): the follow watches the
//!   accumulator's Turn anchor — or, before the submitted message has landed,
//!   the card's chain identity (the unreceived watch: it captures the anchor
//!   the moment the message appears, and ends the card Unreceived when the
//!   session idles first); a Wake continuation watches the chain identity — a
//!   Wake continues the SAME Turn, so the anchor cannot tell its loop apart
//!   from a new Turn's;
//! - **what an ending means**: the follow announces the Turn's true end with a
//!   Completion Notice, while a Wake continuation is itself the notification.
//!   The loop therefore only decides a [`Disposition`]; its callers apply it
//!   through [`Ownership::apply_if_held`] — the shared ending application with
//!   the ownership re-check and the stamp under ONE lock (#539) — and add
//!   their own announcement.
//!
//! Everything else — the reads, the contact bookkeeping, the panel grace, the
//! unreceived watch's waiting hint and the settle decision — lives here once,
//! so a fix to one loop cannot leave the other diverged.

use crate::backend::TurnAnchor;
use crate::bridge::handles::{CardsHandle, FlowHandles};
use crate::opencode::types::SessionStatus;

use super::{SettleTiming, Turn, disposition::Disposition, state};

/// What the loop watches to know it still owns the card. The variant is also
/// the loop's identity: each carries its own label, so a caller cannot pair
/// one loop's guard with the other's name.
pub(super) enum Ownership {
    /// The accumulator's Turn anchor is unchanged: a new Turn (or an external
    /// renderer arming over it) replaces the session's card.
    TurnAnchor(TurnAnchor),
    /// The chain identity is unchanged (see the module docs). `anchor` is the
    /// settle decision's scope: the Turn's own anchor for a chain
    /// continuation, the newest Wake's for a fresh (restart) card.
    Chain { chain: u64, anchor: TurnAnchor },
    /// The unreceived watch (ADR-0062): a follow whose Turn's submitted
    /// message has not landed yet. No Turn anchor exists, so the card's chain
    /// identity is the ownership — it survives a Supplement split and dies
    /// with a new Turn — and the settle decision's anchor is captured from
    /// the transcript the moment `submitted` appears. `hint_at` is when the
    /// neutral waiting line is due: the Turn's start plus the follow grace,
    /// so cola does not nag while a genuine long tool call could still merge
    /// the message.
    Unlanded {
        chain: u64,
        submitted: String,
        hint_at: std::time::Instant,
    },
}

impl Ownership {
    /// Whether the loop still owns the card: the lock + lookup wrapper around
    /// [`Self::held_on`] — the one place the ownership predicate is spelled
    /// (`follow.rs` and the Wake loop both apply through it rather than a
    /// second copy). A missing card is not owned.
    async fn held(&self, cards: &CardsHandle, session_id: &str) -> bool {
        let live = cards.cards.lock().await;
        live.get(session_id).is_some_and(|card| self.held_on(card))
    }

    /// The ownership predicate over an ALREADY-LOCKED card: the accumulator's
    /// Turn anchor is unchanged (a new Turn, or an external renderer arming
    /// over it, replaces the session's card), or — for the chain variants —
    /// the card's chain identity is unchanged (see the module docs). Lock-free
    /// so a check and the write it authorizes can share ONE `cards` lock
    /// (#539), which is what [`Self::apply_if_held`] does.
    fn held_on(&self, card: &state::CardSession) -> bool {
        match self {
            Self::TurnAnchor(anchor) => card.acc.turn_anchor.as_ref() == Some(anchor),
            Self::Chain { chain, .. } | Self::Unlanded { chain, .. } => card.chain_id() == *chain,
        }
    }

    /// Apply the ending `disposition` to `session_id`'s card only while the
    /// loop still owns it — the check and the stamp under ONE `cards` lock
    /// (#539). The callers run after the Session's guard was released, so a
    /// new Turn may replace the card at any moment; a separate re-check would
    /// let the old ending land on the successor's live card in the gap.
    /// Returns whether it applied (false: the card was replaced or vanished —
    /// nothing is touched, and the caller must not announce an ending).
    pub(super) async fn apply_if_held(
        &self,
        cards: &CardsHandle,
        session_id: &str,
        disposition: &Disposition,
    ) -> bool {
        let stamped = {
            let mut live = cards.cards.lock().await;
            match live.get_mut(session_id) {
                Some(card) if self.held_on(card) => {
                    card.acc.apply_ending(disposition);
                    true
                }
                _ => false,
            }
        };
        if !stamped {
            return false;
        }
        Turn::refresh_work_context(cards, session_id).await;
        Turn::flush_card(cards, session_id).await;
        true
    }

    /// The Turn anchor the settle decision reads; `None` for the unreceived
    /// watch until the loop captures the landed message's anchor.
    fn anchor(&self) -> Option<&TurnAnchor> {
        match self {
            Self::TurnAnchor(anchor) | Self::Chain { anchor, .. } => Some(anchor),
            Self::Unlanded { .. } => None,
        }
    }

    /// The submitted message id the unreceived watch captures its anchor
    /// from; `None` for a loop that already has one.
    fn submitted(&self) -> Option<&str> {
        match self {
            Self::Unlanded { submitted, .. } => Some(submitted),
            _ => None,
        }
    }

    /// When the neutral waiting line is due; `None` for a loop whose message
    /// already landed (it never shows the line).
    fn hint_at(&self) -> Option<std::time::Instant> {
        match self {
            Self::Unlanded { hint_at, .. } => Some(*hint_at),
            _ => None,
        }
    }

    /// The loop's name in the bounded-call labels and log lines. Owned by the
    /// variant, so the guard and the name always travel together.
    fn label(&self) -> &'static str {
        match self {
            Self::TurnAnchor(_) | Self::Unlanded { .. } => "drain follow",
            Self::Chain { .. } => "wake continuation",
        }
    }
}

/// Log the ending and hand it back — the one exit for every branch below.
fn finish(disposition: Disposition, session_id: &str, label: &str) -> Option<Disposition> {
    tracing::info!("{label}: session {session_id} {}", disposition.log_line());
    Some(disposition)
}

/// One last reconcile render before a stop terminal (#394): the abort's
/// settled tool states must reach a card that is about to stop updating.
async fn render_once(flow: &FlowHandles, session_id: &str, read_timeout_ms: u64, label: &str) {
    if let Some(Ok(transcript)) = crate::bridge::bounded_call(
        &format!("{label} transcript"),
        read_timeout_ms,
        flow.backend.transcript(session_id),
    )
    .await
    {
        Turn::render_and_flush(
            &flow.cards,
            &flow.sessions,
            &flow.backend,
            &flow.requests,
            session_id,
            &transcript,
        )
        .await;
    }
}

/// Run one out-of-turn settle loop until it no longer owns the card or reaches
/// an ending. `None` means it stopped without an ending: its card was replaced,
/// or the accumulator vanished. Otherwise the returned [`Disposition`] is the
/// one ending the caller applies through [`Ownership::apply_if_held`], which
/// re-checks ownership atomically with the stamp (#539). `owns` carries the
/// loop's name (its variant), which labels the bounded calls and log lines.
pub(super) async fn run(
    flow: &FlowHandles,
    session_id: &str,
    directory: &str,
    timing: SettleTiming,
    owns: &Ownership,
) -> Option<Disposition> {
    let label = owns.label();
    let grace = tokio::time::Duration::from_millis(timing.grace_ms);
    let mut last_contact = tokio::time::Instant::now();
    let mut stuck_since: Option<tokio::time::Instant> = None;
    // The settle decision's scope: the loop's own anchor, or — for the
    // unreceived watch — the anchor captured from the transcript the moment
    // the submitted message lands (ADR-0062). `None` means "never landed", the
    // Unreceived ending.
    let mut anchor: Option<TurnAnchor> = owns.anchor().cloned();
    loop {
        tokio::time::sleep(tokio::time::Duration::from_millis(timing.poll_ms)).await;
        // The card was replaced (a new Turn, an external arming, another Wake
        // continuation): the loop no longer owns it. Never touch it again.
        if !owns.held(&flow.cards, session_id).await {
            return None;
        }
        // `/stop` aborted the run: one last render reconciles the abort's
        // settled tool states, then the stop terminal — a deliberate stop is
        // not a failure and its abort's error never reaches the card (#394).
        if flow.waits.is_stopped(session_id).await {
            render_once(flow, session_id, timing.read_timeout_ms, label).await;
            return finish(Disposition::Stopped, session_id, label);
        }
        // Both reads must answer for the tick to count as contact: a wedged
        // transcript freezes the card and a wedged status hides the ending —
        // each is exactly the "cannot see the run" state the grace bounds.
        let mut in_contact = true;
        let transcript = match crate::bridge::bounded_call(
            &format!("{label} transcript"),
            timing.read_timeout_ms,
            flow.backend.transcript(session_id),
        )
        .await
        {
            Some(Ok(transcript)) => {
                // Stream the parts into the SAME card (the accumulator this
                // chain has been rendering into all along); `None` means the
                // accumulator vanished and the loop no longer owns anything.
                Turn::render_and_flush(
                    &flow.cards,
                    &flow.sessions,
                    &flow.backend,
                    &flow.requests,
                    session_id,
                    &transcript,
                )
                .await?;
                Some(transcript)
            }
            Some(Err(e)) => {
                tracing::warn!("{label} transcript: {e}");
                in_contact = false;
                None
            }
            None => {
                in_contact = false;
                None
            }
        };
        // The unreceived watch's anchor capture (ADR-0062): the submitted
        // message may have landed since the last tick — the render above
        // already captured it onto the card, and from here the normal settle
        // decision judges the Turn. Nothing to do for a loop that already has
        // its anchor, or an anchored read that did not carry the message
        // (compaction cannot un-land it: the local anchor stays Some).
        if anchor.is_none()
            && let (Some(submitted), Some(transcript)) = (owns.submitted(), &transcript)
        {
            anchor = transcript.anchor_of_user(submitted);
        }
        let status = match crate::bridge::bounded_call(
            &format!("{label} session status"),
            timing.read_timeout_ms,
            flow.backend.session_status(session_id, Some(directory)),
        )
        .await
        {
            Some(Ok(status)) => Some(status),
            Some(Err(e)) => {
                tracing::warn!("{label} session status: {e}");
                in_contact = false;
                None
            }
            None => {
                in_contact = false;
                None
            }
        };
        if in_contact {
            last_contact = tokio::time::Instant::now();
        } else if last_contact.elapsed() >= grace {
            return finish(Disposition::LostContact, session_id, label);
        }
        // Still running: keep rendering. There is no total budget; the graces
        // above and below only watch the states nobody can act on.
        if status.is_some_and(|status| status.is_some_and(SessionStatus::is_live)) {
            stuck_since = None;
            // The waiting hint (ADR-0062): while the session reads live but
            // the submitted message has not landed, the card gains the neutral
            // line once the follow grace has passed — a genuine long tool call
            // and a dead run look identical from outside, so cola does not nag
            // early. Pushed once, then flushed.
            if anchor.is_none()
                && owns.hint_at().is_some_and(|at| std::time::Instant::now() >= at)
                && Turn::show_receive_hint(&flow.cards, session_id).await
            {
                Turn::flush_card(&flow.cards, session_id).await;
            }
            continue;
        }
        // The status read itself failed or timed out: observation is broken
        // for this tick, so no ending may be claimed from it — the
        // lost-contact grace above owns the state where it never recovers.
        // (The in-turn drain deliberately reads an unreadable status as idle,
        // the pre-settle rule's own treatment; this loop holds no inflight
        // guard, so "never claim what it cannot see" is the stronger rule
        // here.) An unrecognised status kind still counts as non-busy, like
        // the server's own "absent means idle".
        if status.is_none() {
            stuck_since = None;
            continue;
        }
        // The transcript must have answered too: nothing may be claimed from a
        // read the loop could not make.
        let Some(transcript) = &transcript else {
            stuck_since = None;
            continue;
        };
        // A `⏳` panel must not be stamped over: a crash-orphaned call gets the
        // grace to settle, then ends Error rather than a false `✅`.
        if Turn::has_live_tools(&flow.cards, session_id).await {
            let since = *stuck_since.get_or_insert_with(tokio::time::Instant::now);
            if since.elapsed() >= grace {
                return finish(Disposition::StuckPanel, session_id, label);
            }
            continue;
        }
        // `/stop` may have landed after this tick's own check (the command
        // marks the session before it interrupts, but a tick already past that
        // check can still be classifying the settled run): a deliberate stop
        // is the ending, never the settled Done/Error below (#394). The tick
        // already rendered the abort's settled tool states above.
        if flow.waits.is_stopped(session_id).await {
            return finish(Disposition::Stopped, session_id, label);
        }
        // The single settle decision (ADR-0059, ADR-0062), through the one
        // ending table: an undecided read keeps observing (nothing is
        // stamped), and every decided outcome is that disposition — the
        // caller's shared application owns its card translation.
        let disposition = Disposition::from(transcript.settle(anchor.as_ref()));
        match disposition {
            // A Wake's Execution has not reached its boundary yet: the ending
            // is not decided, and the Wake's content must not declare it.
            Disposition::Observe => stuck_since = None,
            // The Execution ended but Background Tasks are still live: the
            // card yields 「⏳ 等待后台任务」 and stops updating (no Completion
            // Notice — the next Wake continues the chain on a new card).
            // Every other ending — the settled failure, the Unreceived watch
            // (ADR-0062) and the true end — ends the loop here.
            disposition => return finish(disposition, session_id, label),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::test_support::{
        MockBackend, build_app, realistic_parts, test_config, test_work_dir, turn_anchor,
    };
    use crate::feishu::card::CardState;

    /// The card's recorded failure line, read under the cards lock.
    async fn card_error(cards: &CardsHandle, session_id: &str) -> Option<String> {
        cards
            .cards
            .lock()
            .await
            .get(session_id)
            .and_then(|card| card.acc.error.clone())
    }

    /// Seed a recorded failure on the live card, so an ending that clears it
    /// (Stopped) is visible as a change.
    async fn set_card_error(cards: &CardsHandle, session_id: &str, error: &str) {
        if let Some(card) = cards.cards.lock().await.get_mut(session_id) {
            card.acc.error = Some(error.to_string());
        }
    }

    /// #539: the ownership re-check and the ending stamp share ONE cards lock
    /// ([`Ownership::apply_if_held`]), so the released moment between the
    /// settle loop and the stamp cannot hand a successor's live card the old
    /// ending. A stale anchor applies nothing (state and failure untouched,
    /// false); the matching anchor applies (true).
    #[tokio::test]
    async fn apply_if_held_stamps_only_the_anchor_it_still_owns() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let (app, _platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
        let cards = app.cards_handle();

        let live = turn_anchor(1);
        let successor = turn_anchor(2);
        let owns = Ownership::TurnAnchor(live.clone());

        // The ending the loop decided is applied to the card it watched.
        Turn::seed_card(&cards, "ses_test", Some("om_live")).await;
        Turn::set_turn_anchor(&cards, "ses_test", &live).await;
        assert!(
            owns.apply_if_held(&cards, "ses_test", &Disposition::Failed("loop failure".into()))
                .await,
            "the matching anchor applies"
        );
        assert_eq!(Turn::card_state(&cards, "ses_test").await, Some(CardState::Error));
        assert_eq!(
            card_error(&cards, "ses_test").await.as_deref(),
            Some("loop failure")
        );

        // A new Turn replaced the card — and armed its own anchor — while the
        // released moment ran. The stale ending must not clear the successor's
        // recorded failure (Stopped would) nor touch its state.
        Turn::seed_card(&cards, "ses_test", Some("om_successor")).await;
        Turn::set_turn_anchor(&cards, "ses_test", &successor).await;
        Turn::set_card_state(&cards, "ses_test", CardState::Streaming).await;
        set_card_error(&cards, "ses_test", "successor failure").await;
        assert!(
            !owns
                .apply_if_held(&cards, "ses_test", &Disposition::Stopped)
                .await,
            "a stale anchor applies nothing"
        );
        assert_eq!(
            Turn::card_state(&cards, "ses_test").await,
            Some(CardState::Streaming),
            "the successor's state is untouched"
        );
        assert_eq!(
            card_error(&cards, "ses_test").await.as_deref(),
            Some("successor failure"),
            "the successor's recorded failure is untouched"
        );
    }

    /// The chain-identity arm of the same predicate (ADR-0059): a Wake
    /// continuation — and the unreceived watch sharing the variant — applies
    /// its ending only while the card's chain id is unchanged. A replacement
    /// session carries a NEW chain id, so the stale loop stamps nothing; a
    /// vanished card is ownership lost too, and the matching chain applies.
    #[tokio::test]
    async fn apply_if_held_stamps_only_the_chain_it_still_owns() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let (app, _platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
        let cards = app.cards_handle();

        Turn::seed_card(&cards, "ses_test", Some("om_live")).await;
        let chain = Turn::chain_id(&cards, "ses_test").await.expect("seeded card");
        let owns = Ownership::Chain {
            chain,
            anchor: turn_anchor(3),
        };
        assert!(
            owns.apply_if_held(&cards, "ses_test", &Disposition::Done).await,
            "the matching chain applies"
        );
        assert_eq!(Turn::card_state(&cards, "ses_test").await, Some(CardState::Done));

        // A replacement card carries a fresh chain id: the stale loop's ending
        // must not stamp it.
        Turn::seed_card(&cards, "ses_test", Some("om_successor")).await;
        Turn::set_card_state(&cards, "ses_test", CardState::Streaming).await;
        assert!(
            !owns
                .apply_if_held(&cards, "ses_test", &Disposition::Stopped)
                .await,
            "a stale chain applies nothing"
        );
        assert_eq!(
            Turn::card_state(&cards, "ses_test").await,
            Some(CardState::Streaming),
            "the replacement's state is untouched"
        );

        // The card vanishing is ownership lost as well.
        Turn::drop_card(&cards, "ses_test").await;
        assert!(
            !owns.apply_if_held(&cards, "ses_test", &Disposition::Done).await,
            "a vanished card applies nothing"
        );
    }
}
