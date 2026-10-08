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
//! - **what owns the card** ([`Ticket`]): the follow watches the
//!   accumulator's Turn anchor — or, before the submitted message has landed,
//!   the card's chain identity (the unreceived watch: it captures the anchor
//!   the moment the message appears, and ends the card Unreceived when the
//!   session idles first); a Wake continuation watches the chain identity — a
//!   Wake continues the SAME Turn, so the anchor cannot tell its loop apart
//!   from a new Turn's;
//! - **what an ending means**: the follow announces the Turn's true end with a
//!   Completion Notice, while a Wake continuation is itself the notification.
//!   The loop therefore only decides a [`Disposition`]; its callers apply it
//!   through [`Ticket::apply_ending_if_owned`] — the shared ending application
//!   with the ownership re-check and the stamp under ONE lock (#539) — and add
//!   their own announcement.
//!
//! Everything else — the reads, the contact bookkeeping, the panel grace, the
//! unreceived watch's waiting hint and the settle decision — lives here once,
//! so a fix to one loop cannot leave the other diverged.

use crate::backend::TurnAnchor;
use crate::bridge::handles::FlowHandles;
use crate::opencode::types::SessionStatus;

use super::{CardOwnership, SettleTiming, Turn, disposition::Disposition, ownership::Ticket};

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
/// one ending the caller applies through [`Ticket::apply_ending_if_owned`],
/// which re-checks ownership atomically with the stamp (#539). `ticket` carries
/// the loop's identity (its variant), which labels the bounded calls and log
/// lines.
pub(super) async fn run(
    flow: &FlowHandles,
    session_id: &str,
    directory: &str,
    timing: SettleTiming,
    ticket: &Ticket,
) -> Option<Disposition> {
    let label = ticket.label();
    let grace = tokio::time::Duration::from_millis(timing.grace_ms);
    let mut last_contact = tokio::time::Instant::now();
    let mut stuck_since: Option<tokio::time::Instant> = None;
    // The settle decision's scope: the loop's own anchor, or — for the
    // unreceived watch — the anchor captured from the transcript the moment
    // the submitted message lands (ADR-0062). `None` means "never landed", the
    // Unreceived ending.
    let mut anchor: Option<TurnAnchor> = ticket.anchor().cloned();
    loop {
        tokio::time::sleep(tokio::time::Duration::from_millis(timing.poll_ms)).await;
        // The card was replaced (a new Turn, an external arming, another Wake
        // continuation): the ticket no longer covers the live card. Never
        // touch it again. The verdict read is the module's one ownership
        // predicate — the same one the ending application re-checks under the
        // cards lock.
        if !CardOwnership::read(&flow.cards, &flow.waits, session_id)
            .await
            .covers(ticket)
        {
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
        let mut pass = None;
        let transcript = match crate::bridge::bounded_call(
            &format!("{label} transcript"),
            timing.read_timeout_ms,
            flow.backend.transcript(session_id),
        )
        .await
        {
            Some(Ok(mut transcript)) => {
                // The shared Background Task runtime reconciliation (#589) on
                // this tick's own read, before the render and the settle below:
                // a task the runtime confirms ended leaves the live list, its
                // entry renders with the same read, and the settle sees the
                // true end instead of a stranded wait. It observes only once
                // the loop owns an anchor — the card places the entry by it,
                // and a recorded retirement with no anchor would be swallowed
                // by the overlay; the unreceived watch picks the task up on the
                // tick after its message lands and the anchor is captured. The
                // pass is committed below, after the render that carries its
                // entries lands (review, PR #595).
                if anchor.is_some() {
                    pass = flow
                        .runtime_reconcile
                        .observe(
                            &flow.backend,
                            session_id,
                            directory,
                            &mut transcript,
                            timing.read_timeout_ms,
                        )
                        .await;
                }
                // Stream the parts into the SAME card (the accumulator this
                // chain has been rendering into all along); `None` means the
                // accumulator vanished and the loop no longer owns anything.
                let rendered = Turn::render_and_flush(
                    &flow.cards,
                    &flow.sessions,
                    &flow.backend,
                    &flow.requests,
                    session_id,
                    &transcript,
                )
                .await?;
                // The record-after-flush invariant (review, PR #595): the
                // pass commits only once the render that carried its entries
                // returned an accumulator it wrote to AND the flush that
                // carried them was accepted — delivered, or owed by the
                // delivery layer's retry (ADR-0067). A render that never
                // landed, and a permanently refused write, both record
                // nothing — the task stays live and the next card that can
                // render its entry claims it.
                if rendered.flush.accepted()
                    && let Some(pass) = pass.take()
                {
                    pass.commit(&flow.backend, session_id, &transcript);
                }
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
            && let (Some(submitted), Some(transcript)) = (ticket.submitted(), &transcript)
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
                && ticket.hint_at().is_some_and(|at| std::time::Instant::now() >= at)
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
