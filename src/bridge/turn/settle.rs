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
//!   The loop therefore only decides ([`Ending`]) and stamps
//!   ([`stamp`]) — its callers add their own announcement.
//!
//! Everything else — the reads, the contact bookkeeping, the panel grace, the
//! unreceived watch's waiting hint and the settle dispatch — lives here once,
//! so a fix to one loop cannot leave the other diverged.

use crate::backend::{TurnAnchor, TurnSettle};
use crate::bridge::handles::{CardsHandle, FlowHandles};
use crate::opencode::types::SessionStatus;

use super::{SettleTiming, Turn};

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
    /// Whether the loop still owns the card. `pub(super)` for the follow's
    /// exit: after it hands the guard back, it must not stamp a card a new
    /// Turn has taken over in the released moment. The probe stays here (the
    /// one place the ownership predicate is spelled) rather than being
    /// duplicated in `follow.rs`.
    pub(super) async fn held(&self, cards: &CardsHandle, session_id: &str) -> bool {
        match self {
            Self::TurnAnchor(anchor) => {
                Turn::armed_turn_anchor(cards, session_id).await.as_ref() == Some(anchor)
            }
            Self::Chain { chain, .. } | Self::Unlanded { chain, .. } => {
                Turn::chain_id(cards, session_id).await == Some(*chain)
            }
        }
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

/// How the loop stopped observing.
pub(super) enum Ending {
    /// `/stop` marked the session: the deliberate stop's terminal (#394).
    Stopped,
    /// The Execution idled with live Background Tasks: the waiting yield.
    Waiting,
    /// The Turn's settled failure (ADR-0056). The card's kind decides whether
    /// it offers Retry — a Wake continuation never does.
    Failed(String),
    /// The true end: idle with no live Background Task.
    Done,
    /// The submitted message never reached the transcript and the Session is
    /// not live (ADR-0062): the card ends 「⚠️ 这条消息未被接收」 — never ✅.
    Unreceived,
    /// No full read pair answered for the grace: a wedged Backend.
    LostContact,
    /// A readable, settled card still carried a `⏳` panel past the grace.
    StuckPanel,
}

impl Ending {
    /// The one line the loop logs for this ending; the caller's announcement
    /// (if any) follows the stamp.
    fn log_line(&self) -> &'static str {
        match self {
            Self::Stopped => "stopped; finalized Stopped",
            Self::Waiting => "idle with live background tasks; yielded waiting",
            Self::Failed(_) => "failed; finalized Error",
            Self::Done => "idle; finalized",
            Self::Unreceived => "message never landed at idle; finalized Unreceived",
            Self::LostContact => "lost contact; finalized Error",
            Self::StuckPanel => "ended with an unreconcilable panel; finalized Error",
        }
    }
}

/// What a loop records when its reads stopped answering for the grace: the
/// card never sits on an eternal spinner over a Backend it cannot see.
pub(super) const LOST_CONTACT_ERROR: &str = "与运行失去联系，已停止更新。";

/// What a loop records when a readable, settled card still carries a live Tool
/// Panel past the grace (a crash-orphaned call): the card ends Error, never
/// Done over a `⏳` panel.
pub(super) const STUCK_PANEL_ERROR: &str = "运行已结束但工具状态未收尾，已停止更新。";

/// Stamp `session_id`'s card for `ending` — the one ending-to-card mapping
/// both loops share, so a new ending cannot leave one loop's card unstamped.
/// A caller that announces (the follow's Completion Notice) does so after
/// this, reading the card's own terminal.
pub(super) async fn stamp(cards: &CardsHandle, session_id: &str, ending: &Ending) {
    match ending {
        Ending::Stopped => Turn::finalize_stopped(cards, session_id).await,
        Ending::Waiting => Turn::finalize_waiting(cards, session_id).await,
        Ending::Failed(error) => Turn::finalize_error(cards, session_id, error).await,
        Ending::Done => Turn::finalize_done(cards, session_id).await,
        Ending::Unreceived => Turn::finalize_unreceived(cards, session_id).await,
        Ending::LostContact => Turn::finalize_error(cards, session_id, LOST_CONTACT_ERROR).await,
        Ending::StuckPanel => Turn::finalize_error(cards, session_id, STUCK_PANEL_ERROR).await,
    }
}

/// Log the ending and hand it back — the one exit for every branch below.
fn finish(ending: Ending, session_id: &str, label: &str) -> Option<Ending> {
    tracing::info!("{label}: session {session_id} {}", ending.log_line());
    Some(ending)
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
/// or the accumulator vanished. `owns` carries the loop's name (its variant),
/// which labels the bounded calls and log lines.
pub(super) async fn run(
    flow: &FlowHandles,
    session_id: &str,
    directory: &str,
    timing: SettleTiming,
    owns: &Ownership,
) -> Option<Ending> {
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
            return finish(Ending::Stopped, session_id, label);
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
            return finish(Ending::LostContact, session_id, label);
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
                return finish(Ending::StuckPanel, session_id, label);
            }
            continue;
        }
        // `/stop` may have landed after this tick's own check (the command
        // marks the session before it interrupts, but a tick already past that
        // check can still be classifying the settled run): a deliberate stop
        // is the ending, never the settled Done/Error below (#394). The tick
        // already rendered the abort's settled tool states above.
        if flow.waits.is_stopped(session_id).await {
            return finish(Ending::Stopped, session_id, label);
        }
        // The single settle decision (ADR-0059, ADR-0062): this match is
        // exhaustive on purpose, so every decision variant is answered here
        // explicitly.
        match transcript.settle(anchor.as_ref()) {
            // A Wake's Execution has not reached its boundary yet: the ending
            // is not decided, and the Wake's content must not declare it.
            TurnSettle::Running => stuck_since = None,
            // The Execution ended but Background Tasks are still live: the
            // card yields 「⏳ 等待后台任务」 and stops updating (no Completion
            // Notice — the next Wake continues the chain on a new card).
            TurnSettle::Waiting => return finish(Ending::Waiting, session_id, label),
            TurnSettle::Failed(error) => return finish(Ending::Failed(error), session_id, label),
            // The submitted message never landed and the session is idle: the
            // card ends Unreceived — never ✅ (ADR-0062).
            TurnSettle::Unreceived => return finish(Ending::Unreceived, session_id, label),
            TurnSettle::Complete => return finish(Ending::Done, session_id, label),
        }
    }
}
