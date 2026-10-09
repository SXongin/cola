//! The one card ownership verdict (spec #545): who holds a Session's card
//! chain right now, and may a caller touch its card.
//!
//! [`CardOwnership::read`] is the one read. It reads the **waits state**
//! first — the in-flight guard set, then the pending-inbound claim, both
//! unconditionally (an entry past [`crate::bridge::handles`]'
//! `INBOUND_CLAIM_TTL` reads as absent and is dropped by the read, invisibly;
//! a held guard never skips that sweep, so the TTL expiry is uniform across
//! every verdict read) — and then the **card map**, sequentially: each wait
//! lock is released before the next is taken, the card map is taken last, the
//! two are never held together, and the verdict adds no lock of its own.
//!
//! The two sources classify the Session as a PRODUCT (not a mutually
//! exclusive state):
//!
//! - the in-process **claim** ([`Claim`]): none / a message being routed
//!   (inbound) / the guard a Turn or its out-of-turn follow holds;
//! - the current **card class** ([`CardClass`]): absent / render-owned / the
//!   yielded card with its write-readiness / restart-stamped / ended.
//!
//! The product is deliberate: a guard can sit beside a yielded card and a
//! pending message beside a render-owned one, because each call site reads a
//! different rule. The rules are named methods on the verdict:
//!
//! - [`CardOwnership::routing_label`] — the Supplement routing key (ADR-0062):
//!   owned when a guard is held or the card is render-owned; a pending-inbound
//!   claim alone never owns a card to split (the #428 strand), so it routes a
//!   new Turn. The prompt router and the Wake gate read it.
//! - [`CardOwnership::covers`] — the out-of-turn settle ticket's read-time
//!   ownership check (ADR-0059): whether the live card still carries the
//!   [`Ticket`]'s identity. [`Ticket::apply_ending_if_owned`] re-checks through
//!   the same predicate under the cards lock before it stamps (#539), so the
//!   loop's check and its atomic apply cannot disagree.
//! - [`CardOwnership::admits_ledger_refresh`] — the yielded-card write
//!   admission (ADR-0060, ADR-0066): a yielded card, still live, owing no
//!   handoff. The in-place Wake resume, the Background Task Ledger refresh,
//!   the runtime reconciliation's observation and the Wake step's
//!   resume-in-place decision all read this one rule; the two ledger WRITE
//!   sites re-check it under the session's card-write lock through
//!   [`CardOwnership::admit_ledger_write`], so the admission and the write it
//!   authorizes share one lock. The claim half is deliberately irrelevant: the
//!   yield's carve-out is the card's own write-readiness, not the owner's.
//! - [`CardOwnership::reap_claim`] — the durable reap's claim (ADR-0063):
//!   whether any in-process claim holds the Session — the in-flight guard or a
//!   message still being routed, which the reap must count or its PATCH races
//!   the message's admission. The reap pairs it with its own record-relative
//!   probe in the Chain Record module.
//! - [`CardOwnership::stop_disposition`] — the `/stop` acknowledgement's
//!   three-way answer (#394): a render-owned card is stamped by its owner, a
//!   yielded card is acked for the quiet true end it can only reach, and every
//!   other class will never show the stop.
//!
//! The classification carries the identities the rules compare — the card's
//! message id, its Turn anchor and its chain identity — so those rules read
//! one value instead of reopening the map (ADR-0070).
//!
//! Sources, exactly:
//!
//! - the claim: [`WaitsHandle`]'s `inflight` set, then `inbound_pending()`
//!   (which owns the TTL expiry, always read — guard or not);
//! - the card class and the identities: [`CardsHandle`]'s card map — the
//!   [`CardSession`]'s `acc.card_state`, its `card_is_live` + `pending_split`
//!   (the yielded write-readiness), `acc.turn_anchor`, `card_message_id` and
//!   `chain_id()`.

use tokio::sync::{MappedMutexGuard, MutexGuard};

use crate::backend::TurnAnchor;
use crate::bridge::handles::{CardsHandle, WaitsHandle};
use crate::feishu::card::CardState;

use super::Turn;
use super::disposition::Disposition;
use super::state::CardSession;

/// The in-process claim half of the verdict: what THIS process holds for the
/// Session, from the waits state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Claim {
    /// Neither a guard nor a pending message claims the Session.
    None,
    /// A user message is being routed to the Session (#424) and no Turn has
    /// admitted it yet: the waits state's `inbound` claim. Deliberately NOT
    /// ownership for routing — the message has no card to split until it
    /// lands (ADR-0062, the #428 strand).
    Inbound,
    /// The in-flight guard a Turn holds, or the out-of-turn follow that
    /// inherited it for its whole window.
    Guard,
}

/// The current card's class: what the card map says the Session's newest card
/// is, from its state and write-readiness.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CardClass {
    /// The Session has no card.
    Absent,
    /// A live renderer owns the card chain: a Turn (Loading/streaming), a
    /// follow, an external render, or a Wake continuation.
    RenderOwned,
    /// The card yielded to live Background Tasks (ADR-0059). Its
    /// write-readiness is the two fields together: a yielded card accepts the
    /// Background Task Ledger refresh and an in-place Wake resumption only
    /// while it is `live` and owes no split (`handoff_owed` false) — one
    /// class, so the admission's readers cannot disagree about which card is
    /// open (ADR-0060, ADR-0066).
    Yielded {
        /// The card is still the live (growing) card that flushes in place.
        live: bool,
        /// A queued split the chain has not served yet: a handoff is owed, so
        /// the card is no longer open to in-place writes.
        handoff_owed: bool,
    },
    /// The restart-stamped orphan (ADR-0063): no renderer survived the
    /// restart that orphaned the card.
    RestartStamped,
    /// The card reached an ending (terminal).
    Ended,
}

/// What a `/stop` should do about the Session's card (#394), from the
/// verdict's card class: the command's three-way acknowledgement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StopDisposition {
    /// A render-owned card: its owner stamps 「⏹ 已停止」 on its next tick, and
    /// that stamp IS the acknowledgement — the command sends no reply.
    OwnerStamps,
    /// A yielded card: it has no render owner and only reaches the quiet true
    /// end (ADR-0060), so the command acks the deferred settle instead of
    /// staying silent.
    QuietEndAck,
    /// No card, or one whose chain has already spent itself (terminal or
    /// restart-stamped): nothing will ever show the stop, so the command says
    /// so itself.
    Nothing,
}

/// Who owns a Session's card chain right now (ADR-0070): one read's verdict
/// over the waits state and the card map. Fields are private — the named rules
/// and the identities are the interface; the module docs state the sources.
pub(crate) struct CardOwnership {
    claim: Claim,
    card_class: CardClass,
    card_message_id: Option<String>,
    turn_anchor: Option<TurnAnchor>,
    chain_id: Option<u64>,
}

impl CardOwnership {
    /// The one read: the waits state (the guard, then the pending-inbound
    /// claim), then the card map — sequentially, never nested, no new lock.
    ///
    /// Both waits sources are always read, whatever the guard says: the
    /// pending-inbound read owns the TTL sweep, so a held guard must not skip
    /// it — a caller that reports the guard must still leave the expiry
    /// invisible rather than deferred (spec #545). The guard wins the claim's
    /// VALUE, never the read.
    pub(crate) async fn read(cards: &CardsHandle, waits: &WaitsHandle, session_id: &str) -> Self {
        // The guard is read first (it is the stronger claim) and the inbound
        // read runs unconditionally after it, so its TTL sweep cannot depend
        // on which caller happened to hold a guard. A message that is both
        // admitted and still marked inbound reads as the guard — the Turn owns
        // it now.
        let guarded = waits.inflight.lock().await.contains(session_id);
        let inbound = waits.inbound_pending(session_id).await;
        let claim = if guarded {
            Claim::Guard
        } else if inbound {
            Claim::Inbound
        } else {
            Claim::None
        };
        let live = cards.cards.lock().await;
        let Some(card) = live.get(session_id) else {
            return Self {
                claim,
                card_class: CardClass::Absent,
                card_message_id: None,
                turn_anchor: None,
                chain_id: None,
            };
        };
        Self {
            claim,
            card_class: classify(card),
            card_message_id: card.card_message_id.clone(),
            turn_anchor: card.acc.turn_anchor.clone(),
            chain_id: Some(card.chain_id()),
        }
    }

    /// The in-process claim this read found (the waits-state half).
    pub(crate) fn claim(&self) -> Claim {
        self.claim
    }

    /// The card class this read found (the card-map half).
    pub(crate) fn card_class(&self) -> CardClass {
        self.card_class
    }

    /// The card's message id, when a card exists — the identity the reap's
    /// record matching and the settle ticket's `covers` compare. Read by the
    /// reap's claim read (`gather_reads`), so the record match consumes the
    /// same card-map pass the claim classified.
    pub(crate) fn card_message_id(&self) -> Option<&str> {
        self.card_message_id.as_deref()
    }

    /// The card's chain identity, when a card exists — the identity the Wake
    /// continuation loop's ownership guard compares (ADR-0059). Read by the
    /// follow's watch, the three Wake arms' loop guard and the module's tests.
    pub(crate) fn chain_id(&self) -> Option<u64> {
        self.chain_id
    }

    /// The card's Turn anchor, when a card exists — the identity the widened
    /// Wake-step gate compares against the read's newest user message (#568):
    /// Session Sync may resume a yielded card only while it is still the chain
    /// the newest message opened, so a newer external message (which supersedes
    /// and collects the waiting card) is never raced by a Wake resumption.
    pub(crate) fn anchor(&self) -> Option<&TurnAnchor> {
        self.turn_anchor.as_ref()
    }

    /// The Supplement routing key (ADR-0062): `Some(label)` when cola owns the
    /// Session's live card chain — a guard held, or a render-owned card — with
    /// the label the prompt router's INFO line logs (`guard` / `card-chain`),
    /// so the decision and its name come from one value. `None` means a
    /// Supplement would have no card to split: the message starts a new Turn.
    /// A pending-inbound claim alone is deliberately not ownership (the #428
    /// strand): the message is still being routed and owns no card yet.
    pub(crate) fn routing_label(&self) -> Option<&'static str> {
        if self.claim() == Claim::Guard {
            Some("guard")
        } else if self.card_class() == CardClass::RenderOwned {
            Some("card-chain")
        } else {
            None
        }
    }

    /// Whether the live card still carries `ticket`'s identity (ADR-0059): the
    /// out-of-turn settle loop's read-time ownership check.
    /// [`Ticket::apply_ending_if_owned`] re-checks the locked card through the
    /// same [`Ticket::matches`] predicate before it stamps (#539), so the
    /// loop's check and its atomic apply cannot disagree. A missing card
    /// matches nothing.
    pub(crate) fn covers(&self, ticket: &Ticket) -> bool {
        ticket.matches(self.turn_anchor.as_ref(), self.chain_id)
    }

    /// The durable reap's claim (ADR-0063): the Session's card is not orphaned
    /// while this process holds a guard — a Turn, or the out-of-turn follow
    /// that inherited it — or a message is still being routed to it. The
    /// pending claim COUNTS here, unlike the routing key: a message about to
    /// land owns no card yet (the #428 strand), but the reap must wait for it
    /// or its PATCH races the message's admission. The reap pairs this claim
    /// with its own record-relative facts (the Chain Record module's probe) and
    /// never re-reads the waits state.
    pub(crate) fn reap_claim(&self) -> bool {
        self.claim() != Claim::None
    }

    /// The `/stop` disposition (#394): what the command's acknowledgement
    /// reads. A render-owned card is stamped by its owner on the next tick and
    /// needs no reply; a yielded card has no render loop left and only settles
    /// at the quiet true end (ADR-0060), so it is acked; every other class —
    /// no card, a terminal chain, the restart-stamped orphan — can never show
    /// the stop, so the command says nothing ran. The claim half is
    /// deliberately irrelevant: a guard beside the card does not change who
    /// renders it.
    pub(crate) fn stop_disposition(&self) -> StopDisposition {
        match self.card_class() {
            CardClass::RenderOwned => StopDisposition::OwnerStamps,
            CardClass::Yielded { .. } => StopDisposition::QuietEndAck,
            CardClass::Absent | CardClass::RestartStamped | CardClass::Ended => StopDisposition::Nothing,
        }
    }

    /// The yielded-card write admission (ADR-0060, ADR-0066): whether the
    /// Session's card may receive its in-place yield carve-out right now — a
    /// yielded card (Waiting), still live, owing no handoff. The ONE rule the
    /// in-place Wake resume, the Background Task Ledger refresh, the runtime
    /// reconciliation's observation and the Wake step's resume-in-place
    /// decision all read; the claim half is deliberately irrelevant (a guard
    /// can sit beside the yielded card, ADR-0070), and a render-owned,
    /// restart-stamped or ended card never admits.
    pub(crate) fn admits_ledger_refresh(&self) -> bool {
        self.card_class.admits_ledger_refresh()
    }

    /// The lock-scoped re-check for the two ledger WRITE sites (the in-place
    /// Wake resume and the yielded Ledger refresh): the caller holds the
    /// session's card-write lock ([`CardsHandle::write_lock`]); this takes the
    /// card map inside it, re-applies [`Self::admits_ledger_refresh`] to the
    /// live card and hands that card back STILL LOCKED — so the admission and
    /// the write it authorizes share one lock, and no collect, split or ending
    /// can land in a re-lookup gap (#539's property). `None`: the Session has
    /// no card, or its card no longer admits the write. The caller must
    /// release the returned guard before a flush takes the card map again.
    pub(crate) async fn admit_ledger_write<'a>(
        cards: &'a CardsHandle,
        session_id: &str,
    ) -> Option<MappedMutexGuard<'a, CardSession>> {
        let live = cards.cards.lock().await;
        let card = MutexGuard::try_map(live, |map| map.get_mut(session_id)).ok()?;
        admits_ledger_refresh(&card).then_some(card)
    }
}

/// The yielded-card write admission over one card already in hand — the
/// definition [`CardOwnership::admits_ledger_refresh`] and the lock-scoped
/// write helper both read, and the read-time form for a gate that already
/// holds the card map (the Wake step's resume-in-place decision).
pub(super) fn admits_ledger_refresh(card: &CardSession) -> bool {
    classify(card).admits_ledger_refresh()
}

/// The out-of-turn settle loop's ownership ticket (ADR-0059, ADR-0062): what
/// the loop watches to know it still owns the card. The variant is also the
/// loop's identity: each carries its own label, so a caller cannot pair one
/// loop's guard with the other's name. [`super::settle`] keeps the loop; the
/// ticket's identity kinds and its two rule reads
/// ([`CardOwnership::covers`] and [`Self::apply_ending_if_owned`]) live with
/// the verdict.
pub(crate) enum Ticket {
    /// The accumulator's Turn anchor is unchanged: a new Turn (or an external
    /// renderer arming over it) replaces the session's card.
    TurnAnchor(TurnAnchor),
    /// The chain identity is unchanged (see the settle loop's docs). `anchor`
    /// is the settle decision's scope: the Turn's own anchor for a chain
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

impl Ticket {
    /// Whether a card's identities (`anchor`, `chain`) are this ticket's — the
    /// ONE spelling of the ownership predicate (ADR-0059):
    /// [`CardOwnership::covers`] compares the read-time verdict's carried
    /// identity through it, and [`Self::apply_ending_if_owned`] re-checks the
    /// locked card through it, so the loop's check and its atomic apply can
    /// never disagree.
    fn matches(&self, anchor: Option<&TurnAnchor>, chain: Option<u64>) -> bool {
        match self {
            Self::TurnAnchor(own) => anchor == Some(own),
            Self::Chain { chain: own, .. } | Self::Unlanded { chain: own, .. } => chain == Some(*own),
        }
    }

    /// Apply the ending `disposition` to `session_id`'s card only while the
    /// ticket still covers it — the check and the stamp under ONE `cards` lock
    /// (#539). The callers run after the Session's guard was released, so a
    /// new Turn may replace the card at any moment; a separate re-check would
    /// let the old ending land on the successor's live card in the gap.
    /// Returns the identity of the card the ending was STAMPED on (`None`: the
    /// card was replaced, vanished, or carries no id — nothing was applied, and
    /// the caller must not announce an ending). The caller passes this id to
    /// [`crate::bridge::turn::announce_completion`] so the notice is bound to
    /// the very card the ending landed on (spec #602, review finding 6).
    pub(super) async fn apply_ending_if_owned(
        &self,
        cards: &CardsHandle,
        session_id: &str,
        disposition: &Disposition,
    ) -> Option<String> {
        // The chain identity the ending lands on, captured with the stamp under
        // ONE lock: a replacement takes the session over with a FRESH chain, so
        // this is what tells "my own flush split the card onto a continuation"
        // (same chain) from "a new Turn replaced it" (new chain).
        let chain = {
            let mut live = cards.cards.lock().await;
            match live.get_mut(session_id) {
                Some(card) if self.matches(card.acc.turn_anchor.as_ref(), Some(card.chain_id())) => {
                    card.acc.apply_ending(disposition);
                    card.chain_id()
                }
                _ => return None,
            }
        };
        Turn::refresh_work_context(cards, session_id).await;
        Turn::flush_card(cards, session_id).await;
        // The card the ending landed on, read AFTER the flush (spec #602, review
        // finding 6; ticket #607): our own terminal flush may size-split the
        // slice onto a continuation — the SAME chain — and the notice must bind
        // to the continuation, not the finalized prefix it replaced (read as a
        // "newer Turn replaced the card", that would suppress the notice). A new
        // Turn's fresh chain means the ending's card is gone: stay silent rather
        // than announce over the replacement.
        let live = cards.cards.lock().await;
        match live.get(session_id) {
            Some(card) if card.chain_id() == chain => card.card_message_id.clone(),
            _ => None,
        }
    }

    /// The Turn anchor the settle decision reads; `None` for the unreceived
    /// watch until the loop captures the landed message's anchor.
    pub(super) fn anchor(&self) -> Option<&TurnAnchor> {
        match self {
            Self::TurnAnchor(anchor) | Self::Chain { anchor, .. } => Some(anchor),
            Self::Unlanded { .. } => None,
        }
    }

    /// The submitted message id the unreceived watch captures its anchor
    /// from; `None` for a loop that already has one.
    pub(super) fn submitted(&self) -> Option<&str> {
        match self {
            Self::Unlanded { submitted, .. } => Some(submitted),
            _ => None,
        }
    }

    /// When the neutral waiting line is due; `None` for a loop whose message
    /// already landed (it never shows the line).
    pub(super) fn hint_at(&self) -> Option<std::time::Instant> {
        match self {
            Self::Unlanded { hint_at, .. } => Some(*hint_at),
            _ => None,
        }
    }

    /// The loop's name in the bounded-call labels and log lines. Owned by the
    /// variant, so the guard and the name always travel together.
    pub(super) fn label(&self) -> &'static str {
        match self {
            Self::TurnAnchor(_) | Self::Unlanded { .. } => "drain follow",
            Self::Chain { .. } => "wake continuation",
        }
    }
}

/// The card class from one [`CardSession`]: the display state first, then the
/// yielded card's write-readiness.
fn classify(card: &CardSession) -> CardClass {
    let state = card.acc.card_state();
    if *state == CardState::Waiting {
        CardClass::Yielded {
            live: card.card_is_live,
            handoff_owed: !card.pending_split.is_empty(),
        }
    } else if *state == CardState::Restarted {
        CardClass::RestartStamped
    } else if state.is_terminal() {
        CardClass::Ended
    } else {
        CardClass::RenderOwned
    }
}

impl CardClass {
    /// The yielded-card write admission's one definition (ADR-0060,
    /// ADR-0066): exactly the OPEN yielded card — still live, owing no
    /// handoff. Every other class refuses: a render-owned card belongs to its
    /// renderer, and a restart-stamped or ended card has spent its chain.
    fn admits_ledger_refresh(self) -> bool {
        matches!(
            self,
            Self::Yielded {
                live: true,
                handoff_owed: false
            }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::test_support::{
        MockBackend, build_app, realistic_parts, test_config, test_work_dir, turn_anchor,
    };
    use crate::bridge::turn::{SplitKind, Turn};

    const SID: &str = "ses_test";

    /// The module's shared fixture: an isolated work dir (cwd — sessions are
    /// created there, and the guard keeps it alive), a temp session store and
    /// the mock platform backend, reduced to the two handles the verdict
    /// reads. Every test here needs exactly this quartet; building it in one
    /// place keeps the fixtures from drifting.
    struct Fixture {
        cards: CardsHandle,
        waits: WaitsHandle,
        _work_dir: tempfile::TempDir,
        _store_dir: tempfile::TempDir,
    }

    /// Build one module fixture: the app over a fresh work dir and session
    /// store, with the card and waits handles.
    async fn fixture() -> Fixture {
        let work_dir = test_work_dir();
        let store_dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&store_dir.path().join("sessions.json"));
        let (app, _platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
        Fixture {
            cards: app.cards_handle(),
            waits: app.waits_handle(),
            _work_dir: work_dir,
            _store_dir: store_dir,
        }
    }

    /// Set the waits state to exactly `claim` (clearing both sources first).
    async fn set_claim(waits: &WaitsHandle, claim: Claim) {
        waits.inflight.lock().await.remove(SID);
        waits.clear_inbound(SID).await;
        match claim {
            Claim::None => {}
            Claim::Inbound => waits.note_inbound(SID).await,
            Claim::Guard => {
                waits.inflight.lock().await.insert(SID.to_string());
            }
        }
    }

    /// Give the Session the card `state` — armed with the fixed Turn anchor
    /// the table's `covers` row reads — or no card at all.
    async fn set_card(cards: &CardsHandle, state: Option<CardState>) {
        match state {
            None => Turn::drop_card(cards, SID).await,
            Some(state) => {
                Turn::seed_card(cards, SID, Some("om_live")).await;
                Turn::set_turn_anchor(cards, SID, &turn_anchor(1)).await;
                Turn::set_card_state(cards, SID, state).await;
            }
        }
    }

    /// The verdict's complete table (ADR-0070): every `CardState` × claim
    /// combination, read through ONE `CardOwnership::read`, pins every named
    /// rule — the class, the routing rule with its label, the yielded write
    /// admission, the reap's claim, `covers` and the `/stop` disposition.
    ///
    /// The product is deliberate: the claim is carried through untouched (a
    /// guard beside a yielded card, a pending message beside a render-owned
    /// one), the claim-only rules read apart on purpose (the reap counts the
    /// pending message; the routing key ignores it — the #428 strand), and the
    /// card-only rules are constant under every claim. `covers` follows the
    /// card's own armed anchor.
    #[tokio::test]
    async fn classification_covers_every_card_state_under_every_claim() {
        let fx = fixture().await;
        let cards = &fx.cards;
        let waits = &fx.waits;

        // Every CardState the map can hold, with the card half of every named
        // rule: its class, the routing label a NON-guard claim sees (a guard's
        // own label is asserted in the loop), the yielded write admission and
        // the `/stop` disposition. `None` is the map's absent key.
        // (state, class, non-guard routing label, admission, /stop disposition)
        type Row = (
            Option<CardState>,
            CardClass,
            Option<&'static str>,
            bool,
            StopDisposition,
        );
        let rows: &[Row] = &[
            (None, CardClass::Absent, None, false, StopDisposition::Nothing),
            (
                Some(CardState::Loading),
                CardClass::RenderOwned,
                Some("card-chain"),
                false,
                StopDisposition::OwnerStamps,
            ),
            (
                Some(CardState::Reasoning),
                CardClass::RenderOwned,
                Some("card-chain"),
                false,
                StopDisposition::OwnerStamps,
            ),
            (
                Some(CardState::Streaming),
                CardClass::RenderOwned,
                Some("card-chain"),
                false,
                StopDisposition::OwnerStamps,
            ),
            (
                Some(CardState::Continued),
                CardClass::RenderOwned,
                Some("card-chain"),
                false,
                StopDisposition::OwnerStamps,
            ),
            (
                Some(CardState::Resuming),
                CardClass::RenderOwned,
                Some("card-chain"),
                false,
                StopDisposition::OwnerStamps,
            ),
            (
                Some(CardState::Waiting),
                CardClass::Yielded {
                    live: true,
                    handoff_owed: false,
                },
                None,
                true,
                StopDisposition::QuietEndAck,
            ),
            (
                Some(CardState::Restarted),
                CardClass::RestartStamped,
                None,
                false,
                StopDisposition::Nothing,
            ),
            (
                Some(CardState::Done),
                CardClass::Ended,
                None,
                false,
                StopDisposition::Nothing,
            ),
            (
                Some(CardState::Error),
                CardClass::Ended,
                None,
                false,
                StopDisposition::Nothing,
            ),
            (
                Some(CardState::Retried),
                CardClass::Ended,
                None,
                false,
                StopDisposition::Nothing,
            ),
            (
                Some(CardState::Stopped),
                CardClass::Ended,
                None,
                false,
                StopDisposition::Nothing,
            ),
            (
                Some(CardState::Unreceived),
                CardClass::Ended,
                None,
                false,
                StopDisposition::Nothing,
            ),
            (
                Some(CardState::Superseded),
                CardClass::Ended,
                None,
                false,
                StopDisposition::Nothing,
            ),
            (
                Some(CardState::SwitchedAway),
                CardClass::Ended,
                None,
                false,
                StopDisposition::Nothing,
            ),
            (
                Some(CardState::TakenOver),
                CardClass::Ended,
                None,
                false,
                StopDisposition::Nothing,
            ),
        ];

        for (state, expected_class, expected_routing, expected_admission, expected_stop) in rows {
            for claim in [Claim::None, Claim::Inbound, Claim::Guard] {
                set_claim(waits, claim).await;
                set_card(cards, state.clone()).await;

                let ownership = CardOwnership::read(cards, waits, SID).await;
                assert_eq!(
                    ownership.claim(),
                    claim,
                    "claim must be carried untouched for {state:?}"
                );
                assert_eq!(
                    ownership.card_class(),
                    *expected_class,
                    "class of {state:?} under {claim:?}"
                );
                // A guard always owns the session, and its label wins when the
                // card is render-owned too; otherwise the card chain decides.
                let routing = if claim == Claim::Guard {
                    Some("guard")
                } else {
                    *expected_routing
                };
                assert_eq!(
                    ownership.routing_label(),
                    routing,
                    "routing of {state:?} under {claim:?}"
                );
                assert_eq!(
                    ownership.admits_ledger_refresh(),
                    *expected_admission,
                    "admission of {state:?} under {claim:?}"
                );
                // The reap counts the pending message too — it must not PATCH
                // a card the message is about to reach (ADR-0063).
                assert_eq!(
                    ownership.reap_claim(),
                    claim != Claim::None,
                    "reap claim of {state:?} under {claim:?}"
                );
                // The card's own armed anchor is covered exactly while the
                // card exists; a missing card is covered by nothing (ADR-0059).
                assert_eq!(
                    ownership.covers(&Ticket::TurnAnchor(turn_anchor(1))),
                    state.is_some(),
                    "covers of {state:?} under {claim:?}"
                );
                assert_eq!(
                    ownership.stop_disposition(),
                    *expected_stop,
                    "/stop disposition of {state:?} under {claim:?}"
                );
            }
        }
    }

    /// The yielded class carries the card's LIVE write-readiness: still the
    /// growing card with no handoff owed is open; a queued split or a card no
    /// longer live is not (ADR-0060, ADR-0066). The admission follows the
    /// class exactly.
    #[tokio::test]
    async fn yielded_classifies_its_write_readiness() {
        let fx = fixture().await;
        let cards = &fx.cards;
        let waits = &fx.waits;

        Turn::seed_card(cards, SID, Some("om_live")).await;
        Turn::set_card_state(cards, SID, CardState::Waiting).await;
        let open = CardOwnership::read(cards, waits, SID).await;
        assert_eq!(
            open.card_class(),
            CardClass::Yielded {
                live: true,
                handoff_owed: false
            },
            "a yielded card that is live and owes nothing is open"
        );
        assert!(
            open.admits_ledger_refresh(),
            "the open yielded card admits its in-place writes"
        );

        // A queued split the chain has not served: the handoff is owed.
        cards.cards.lock().await.get_mut(SID).unwrap().pending_split.push(
            crate::bridge::turn::state::PendingSplit {
                reply_to: "om_user".into(),
                kind: SplitKind::Supplement,
                receipt_pushed: false,
                line: None,
                handover: false,
            },
        );
        let owed = CardOwnership::read(cards, waits, SID).await;
        assert_eq!(
            owed.card_class(),
            CardClass::Yielded {
                live: true,
                handoff_owed: true
            },
            "an owed handoff closes the card to in-place writes"
        );
        assert!(
            !owed.admits_ledger_refresh(),
            "an owed handoff refuses the in-place writes"
        );

        // A card a split already finalized is no longer the live one.
        cards.cards.lock().await.get_mut(SID).unwrap().card_is_live = false;
        let finalized = CardOwnership::read(cards, waits, SID).await;
        assert_eq!(
            finalized.card_class(),
            CardClass::Yielded {
                live: false,
                handoff_owed: true
            },
            "a finalized card is not the live one"
        );
        assert!(
            !finalized.admits_ledger_refresh(),
            "a finalized card refuses the in-place writes"
        );
    }

    /// The lock-scoped write admission ([`CardOwnership::admit_ledger_write`],
    /// the two ledger write sites' re-check) hands back exactly the open
    /// yielded card and leaves it LOCKED from the card map — so the admission
    /// and the write it authorizes share one lock (#539). A render-owned card,
    /// a terminal card, a restart-stamped card, an owed handoff, a finalized
    /// card and a missing card are all refused.
    #[tokio::test]
    async fn locked_write_admission_hands_back_only_the_open_yielded_card() {
        let fx = fixture().await;
        let cards = &fx.cards;

        // No card at all: nothing to admit.
        assert!(CardOwnership::admit_ledger_write(cards, SID).await.is_none());

        // The open yielded card: admitted and still locked, so the write
        // through the returned guard cannot race the admission.
        Turn::seed_card(cards, SID, Some("om_live")).await;
        Turn::set_card_state(cards, SID, CardState::Waiting).await;
        let mut card = CardOwnership::admit_ledger_write(cards, SID)
            .await
            .expect("the open yielded card admits its in-place writes");
        assert!(
            cards.cards.try_lock().is_err(),
            "the admitted card is handed back still under the card map's lock"
        );
        card.card_is_live = false;
        drop(card);
        assert!(
            cards.cards.try_lock().is_ok(),
            "releasing the admitted guard releases the card map"
        );
        assert!(
            !cards.cards.lock().await.get(SID).unwrap().card_is_live,
            "the write through the admitted card landed"
        );

        // A live card is render-owned: its own renderer owns the writes.
        Turn::seed_card(cards, SID, Some("om_live")).await;
        Turn::set_card_state(cards, SID, CardState::Streaming).await;
        assert!(CardOwnership::admit_ledger_write(cards, SID).await.is_none());

        // A terminal card keeps the ending it recorded.
        Turn::set_card_state(cards, SID, CardState::Done).await;
        assert!(CardOwnership::admit_ledger_write(cards, SID).await.is_none());

        // The restart-stamped orphan is spent too.
        Turn::set_card_state(cards, SID, CardState::Restarted).await;
        assert!(CardOwnership::admit_ledger_write(cards, SID).await.is_none());

        // A queued split closes the yielded card: the handoff is owed.
        Turn::seed_card(cards, SID, Some("om_live")).await;
        Turn::set_card_state(cards, SID, CardState::Waiting).await;
        cards.cards.lock().await.get_mut(SID).unwrap().pending_split.push(
            crate::bridge::turn::state::PendingSplit {
                reply_to: "om_user".into(),
                kind: SplitKind::Supplement,
                receipt_pushed: false,
                line: None,
                handover: false,
            },
        );
        assert!(CardOwnership::admit_ledger_write(cards, SID).await.is_none());

        // A split already finalized the card: it is no longer the live one.
        cards.cards.lock().await.get_mut(SID).unwrap().card_is_live = false;
        assert!(CardOwnership::admit_ledger_write(cards, SID).await.is_none());
    }

    /// The read carries the identities the remaining rules compare: the card's
    /// message id, its Turn anchor and its chain identity — all absent when
    /// the Session has no card.
    #[tokio::test]
    async fn read_carries_the_cards_identities() {
        let fx = fixture().await;
        let cards = &fx.cards;
        let waits = &fx.waits;

        let anchor = turn_anchor(7);
        Turn::seed_card(cards, SID, Some("om_live")).await;
        Turn::set_card_state(cards, SID, CardState::Streaming).await;
        Turn::set_turn_anchor(cards, SID, &anchor).await;
        let chain = cards.cards.lock().await.get(SID).expect("seeded card").chain_id();

        let ownership = CardOwnership::read(cards, waits, SID).await;
        assert_eq!(ownership.card_message_id(), Some("om_live"));
        // The carried anchor has no accessor — the ticket's `covers` compares
        // it inside the verdict — so the test reads the field directly.
        assert_eq!(ownership.turn_anchor.as_ref(), Some(&anchor));
        assert_eq!(ownership.chain_id(), Some(chain));

        Turn::drop_card(cards, SID).await;
        let absent = CardOwnership::read(cards, waits, SID).await;
        assert_eq!(absent.card_message_id(), None);
        assert_eq!(absent.turn_anchor.as_ref(), None);
        assert_eq!(absent.chain_id(), None);
    }

    /// A stale inbound claim reads as absent and its entry is dropped by the
    /// read — the TTL expiry is invisible (the claim a caller sees is "none",
    /// never an error), and uniform (every verdict read applies it).
    #[tokio::test]
    async fn an_expired_inbound_claim_reads_as_absent_and_is_dropped() {
        let fx = fixture().await;
        let cards = &fx.cards;
        let waits = &fx.waits;

        // The claim TTL is 120s (`INBOUND_CLAIM_TTL`); age this entry past it.
        waits.inbound.lock().await.insert(
            SID.to_string(),
            std::time::Instant::now() - std::time::Duration::from_secs(121),
        );
        let ownership = CardOwnership::read(cards, waits, SID).await;
        assert_eq!(ownership.claim(), Claim::None);
        assert!(
            !waits.inbound.lock().await.contains_key(SID),
            "the expired entry is dropped by the read"
        );
    }

    /// A held guard never skips the pending-inbound read: the TTL sweep runs
    /// under it too — the guard only wins the claim VALUE — so an expired
    /// entry cannot survive a verdict read that reported a guard (#545). The
    /// expiry stays invisible (the claim never changes), but it stays uniform:
    /// the reap's `reap_claim`, reading after a guard slipped in, must not
    /// count a claim the TTL already spent.
    #[tokio::test]
    async fn a_held_guard_still_sweeps_an_expired_inbound_claim() {
        let fx = fixture().await;
        let cards = &fx.cards;
        let waits = &fx.waits;

        waits.inflight.lock().await.insert(SID.to_string());
        waits.inbound.lock().await.insert(
            SID.to_string(),
            std::time::Instant::now() - std::time::Duration::from_secs(121),
        );

        let ownership = CardOwnership::read(cards, waits, SID).await;
        assert_eq!(ownership.claim(), Claim::Guard, "the guard is the claim value");
        assert!(
            !waits.inbound.lock().await.contains_key(SID),
            "the read sweeps the expired entry even while a guard is held"
        );
    }

    /// The read-time ownership rule (ADR-0059): [`CardOwnership::covers`]
    /// compares the verdict's carried identity against the ticket's variant —
    /// each kind matches only its own identity, and a missing card matches
    /// nothing. The lock-scoped [`Ticket::apply_ending_if_owned`] re-checks
    /// through the same predicate, so the loop's read-time check and its
    /// atomic apply cannot disagree.
    #[tokio::test]
    async fn covers_compares_each_ticket_variant_against_the_live_card() {
        let fx = fixture().await;
        let cards = &fx.cards;
        let waits = &fx.waits;

        let anchor = turn_anchor(1);
        Turn::seed_card(cards, SID, Some("om_live")).await;
        Turn::set_turn_anchor(cards, SID, &anchor).await;
        let chain = cards.cards.lock().await.get(SID).expect("seeded card").chain_id();
        let unlanded = |chain| Ticket::Unlanded {
            chain,
            submitted: "om_user".into(),
            hint_at: std::time::Instant::now(),
        };

        let ownership = CardOwnership::read(cards, waits, SID).await;
        assert!(
            ownership.covers(&Ticket::TurnAnchor(anchor.clone())),
            "the armed anchor is covered"
        );
        assert!(
            !ownership.covers(&Ticket::TurnAnchor(turn_anchor(2))),
            "another anchor is not"
        );
        assert!(
            ownership.covers(&Ticket::Chain {
                chain,
                anchor: turn_anchor(9),
            }),
            "the live chain is covered"
        );
        assert!(
            !ownership.covers(&Ticket::Chain {
                chain: chain + 1,
                anchor: turn_anchor(9),
            }),
            "another chain is not"
        );
        assert!(
            ownership.covers(&unlanded(chain)),
            "the unlanded watch's chain is covered"
        );
        assert!(!ownership.covers(&unlanded(chain + 1)), "another chain is not");

        // The card vanishes: nothing is covered any more.
        Turn::drop_card(cards, SID).await;
        let gone = CardOwnership::read(cards, waits, SID).await;
        assert!(
            !gone.covers(&Ticket::TurnAnchor(anchor)) && !gone.covers(&unlanded(chain)),
            "a missing card is covered by nothing"
        );
    }

    /// The card's recorded failure line, read under the cards lock.
    async fn card_error(cards: &CardsHandle, session_id: &str) -> Option<String> {
        cards
            .cards
            .lock()
            .await
            .get(session_id)
            .and_then(|card| card.acc.error().map(str::to_string))
    }

    /// Seed a recorded failure on the live card, so an ending that clears it
    /// (Stopped) is visible as a change.
    async fn set_card_error(cards: &CardsHandle, session_id: &str, error: &str) {
        if let Some(card) = cards.cards.lock().await.get_mut(session_id) {
            card.acc.set_error(Some(error.to_string()));
        }
    }

    /// #539: the ownership re-check and the ending stamp share ONE cards lock
    /// ([`Ticket::apply_ending_if_owned`]), so the released moment between the
    /// settle loop and the stamp cannot hand a successor's live card the old
    /// ending. A stale anchor applies nothing (state and failure untouched,
    /// `None`); the matching anchor applies and returns its card id (`Some`).
    #[tokio::test]
    async fn apply_ending_if_owned_stamps_only_the_anchor_it_still_owns() {
        let fx = fixture().await;
        let cards = &fx.cards;

        let live = turn_anchor(1);
        let successor = turn_anchor(2);
        let owns = Ticket::TurnAnchor(live.clone());

        // The ending the loop decided is applied to the card it watched.
        Turn::seed_card(cards, SID, Some("om_live")).await;
        Turn::set_turn_anchor(cards, SID, &live).await;
        assert_eq!(
            owns.apply_ending_if_owned(cards, SID, &Disposition::Failed("loop failure".into()))
                .await
                .as_deref(),
            Some("om_live"),
            "the matching anchor applies to its own card"
        );
        assert_eq!(Turn::card_state(cards, SID).await, Some(CardState::Error));
        assert_eq!(card_error(cards, SID).await.as_deref(), Some("loop failure"));

        // A new Turn replaced the card — and armed its own anchor — while the
        // released moment ran. The stale ending must not clear the successor's
        // recorded failure (Stopped would) nor touch its state.
        Turn::seed_card(cards, SID, Some("om_successor")).await;
        Turn::set_turn_anchor(cards, SID, &successor).await;
        Turn::set_card_state(cards, SID, CardState::Streaming).await;
        set_card_error(cards, SID, "successor failure").await;
        assert!(
            owns.apply_ending_if_owned(cards, SID, &Disposition::Stopped)
                .await
                .is_none(),
            "a stale anchor applies nothing"
        );
        assert_eq!(
            Turn::card_state(cards, SID).await,
            Some(CardState::Streaming),
            "the successor's state is untouched"
        );
        assert_eq!(
            card_error(cards, SID).await.as_deref(),
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
    async fn apply_ending_if_owned_stamps_only_the_chain_it_still_owns() {
        let fx = fixture().await;
        let cards = &fx.cards;

        Turn::seed_card(cards, SID, Some("om_live")).await;
        let chain = cards.cards.lock().await.get(SID).expect("seeded card").chain_id();
        let owns = Ticket::Chain {
            chain,
            anchor: turn_anchor(3),
        };
        assert_eq!(
            owns.apply_ending_if_owned(cards, SID, &Disposition::Done)
                .await
                .as_deref(),
            Some("om_live"),
            "the matching chain applies to its own card"
        );
        assert_eq!(Turn::card_state(cards, SID).await, Some(CardState::Done));

        // A replacement card carries a fresh chain id: the stale loop's ending
        // must not stamp it.
        Turn::seed_card(cards, SID, Some("om_successor")).await;
        Turn::set_card_state(cards, SID, CardState::Streaming).await;
        assert!(
            owns.apply_ending_if_owned(cards, SID, &Disposition::Stopped)
                .await
                .is_none(),
            "a stale chain applies nothing"
        );
        assert_eq!(
            Turn::card_state(cards, SID).await,
            Some(CardState::Streaming),
            "the replacement's state is untouched"
        );

        // The card vanishing is ownership lost as well.
        Turn::drop_card(cards, SID).await;
        assert!(
            owns.apply_ending_if_owned(cards, SID, &Disposition::Done)
                .await
                .is_none(),
            "a vanished card applies nothing"
        );
    }
}
