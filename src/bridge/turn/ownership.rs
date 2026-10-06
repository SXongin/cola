//! The one card ownership verdict (spec #545): who holds a Session's card
//! chain right now, and may a caller touch its card.
//!
//! [`CardOwnership::read`] is the one read. It reads the **waits state**
//! first — the in-flight guard set, then the pending-inbound claim (an entry
//! past [`crate::bridge::handles`]' `INBOUND_CLAIM_TTL` reads as absent and is
//! dropped, invisibly) — and then the **card map**, sequentially: each wait
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
//!
//! The classification carries the identities the remaining rules need — the
//! card's message id, its Turn anchor and its chain identity — so those rules
//! land as methods over one value, not as new reads. Later tickets of the spec
//! consume the yielded-card write admission, the reap's claim, the settle
//! ticket and the `/stop` disposition (ADR-0070).
//!
//! Sources, exactly:
//!
//! - the claim: [`WaitsHandle`]'s `inflight` set, then `inbound_pending()`
//!   (which owns the TTL expiry);
//! - the card class and the identities: [`CardsHandle`]'s card map — the
//!   [`CardSession`]'s `acc.card_state`, its `card_is_live` + `pending_split`
//!   (the yielded write-readiness), `acc.turn_anchor`, `card_message_id` and
//!   `chain_id()`.

use crate::backend::TurnAnchor;
use crate::bridge::handles::{CardsHandle, WaitsHandle};
use crate::feishu::card::CardState;

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
    /// The one read: the waits state (guard, then the pending-inbound claim),
    /// then the card map — sequentially, never nested, no new lock.
    pub(crate) async fn read(cards: &CardsHandle, waits: &WaitsHandle, session_id: &str) -> Self {
        // The guard first: a held guard is the strongest claim, and a
        // message that is both admitted and still marked inbound reads as
        // the guard (the Turn owns it now). `inbound_pending` owns the TTL
        // rule — an expired claim reads as absent and is dropped, invisible
        // to every caller.
        let claim = if waits.inflight.lock().await.contains(session_id) {
            Claim::Guard
        } else if waits.inbound_pending(session_id).await {
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
    /// record matching and the settle ticket's `covers` compare.
    #[allow(dead_code)] // consumed by spec #545's later tickets; the module table test reads it today
    pub(crate) fn card_message_id(&self) -> Option<&str> {
        self.card_message_id.as_deref()
    }

    /// The card's Turn anchor, when one is armed — the identity the settle
    /// ticket's anchor variant compares (ADR-0059).
    #[allow(dead_code)] // consumed by spec #545's later tickets; the module table test reads it today
    pub(crate) fn turn_anchor(&self) -> Option<&TurnAnchor> {
        self.turn_anchor.as_ref()
    }

    /// The card's chain identity, when a card exists — the identity a Wake
    /// continuation loop's ownership guard compares (ADR-0059).
    #[allow(dead_code)] // consumed by spec #545's later tickets; the module table test reads it today
    pub(crate) fn chain_id(&self) -> Option<u64> {
        self.chain_id
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
}

/// The card class from one [`CardSession`]: the display state first, then the
/// yielded card's write-readiness.
fn classify(card: &CardSession) -> CardClass {
    let state = &card.acc.card_state;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::test_support::{
        MockBackend, build_app, realistic_parts, test_config, test_work_dir, turn_anchor,
    };
    use crate::bridge::turn::{SplitKind, Turn};

    const SID: &str = "ses_test";

    /// Set the waits state to exactly `claim` (clearing both sources first).
    async fn set_claim(app: &std::sync::Arc<crate::bridge::handler::App>, claim: Claim) {
        let waits = app.waits_handle();
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

    /// Give the Session the card `state`, or no card at all.
    async fn set_card(cards: &CardsHandle, state: Option<CardState>) {
        match state {
            None => Turn::drop_card(cards, SID).await,
            Some(state) => {
                Turn::seed_card(cards, SID, Some("om_live")).await;
                Turn::set_card_state(cards, SID, state).await;
            }
        }
    }

    /// The classification is the product of the two halves: every `CardState`
    /// reads as its class under every claim, and the claim is carried through
    /// untouched (a guard beside a yielded card, a pending message beside a
    /// render-owned one — ADR-0070).
    #[tokio::test]
    async fn classification_covers_every_card_state_under_every_claim() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let (app, _platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
        let cards = app.cards_handle();
        let waits = app.waits_handle();

        // Every CardState the map can hold, with the class it must read as.
        // `None` is the map's absent key.
        let states: &[(Option<CardState>, CardClass)] = &[
            (None, CardClass::Absent),
            (Some(CardState::Loading), CardClass::RenderOwned),
            (Some(CardState::Reasoning), CardClass::RenderOwned),
            (Some(CardState::Streaming), CardClass::RenderOwned),
            (Some(CardState::Continued), CardClass::RenderOwned),
            (Some(CardState::Resuming), CardClass::RenderOwned),
            (
                Some(CardState::Waiting),
                CardClass::Yielded {
                    live: true,
                    handoff_owed: false,
                },
            ),
            (Some(CardState::Restarted), CardClass::RestartStamped),
            (Some(CardState::Done), CardClass::Ended),
            (Some(CardState::Error), CardClass::Ended),
            (Some(CardState::Retried), CardClass::Ended),
            (Some(CardState::Stopped), CardClass::Ended),
            (Some(CardState::Unreceived), CardClass::Ended),
            (Some(CardState::Superseded), CardClass::Ended),
            (Some(CardState::SwitchedAway), CardClass::Ended),
            (Some(CardState::TakenOver), CardClass::Ended),
        ];

        for (state, expected_class) in states {
            for claim in [Claim::None, Claim::Inbound, Claim::Guard] {
                set_claim(&app, claim).await;
                set_card(&cards, state.clone()).await;

                let ownership = CardOwnership::read(&cards, &waits, SID).await;
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
            }
        }
    }

    /// The yielded class carries the card's LIVE write-readiness: still the
    /// growing card with no handoff owed is open; a queued split or a card no
    /// longer live is not (ADR-0060, ADR-0066).
    #[tokio::test]
    async fn yielded_classifies_its_write_readiness() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let (app, _platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
        let cards = app.cards_handle();
        let waits = app.waits_handle();

        Turn::seed_card(&cards, SID, Some("om_live")).await;
        Turn::set_card_state(&cards, SID, CardState::Waiting).await;
        let open = CardOwnership::read(&cards, &waits, SID).await;
        assert_eq!(
            open.card_class(),
            CardClass::Yielded {
                live: true,
                handoff_owed: false
            },
            "a yielded card that is live and owes nothing is open"
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
        let owed = CardOwnership::read(&cards, &waits, SID).await;
        assert_eq!(
            owed.card_class(),
            CardClass::Yielded {
                live: true,
                handoff_owed: true
            },
            "an owed handoff closes the card to in-place writes"
        );

        // A card a split already finalized is no longer the live one.
        cards.cards.lock().await.get_mut(SID).unwrap().card_is_live = false;
        let finalized = CardOwnership::read(&cards, &waits, SID).await;
        assert_eq!(
            finalized.card_class(),
            CardClass::Yielded {
                live: false,
                handoff_owed: true
            },
            "a finalized card is not the live one"
        );
    }

    /// The routing rule (ADR-0062) and its label: a guard or a render-owned
    /// card owns; every other combination — a pending-inbound claim alone
    /// included (the #428 strand) — starts a new Turn. The guard's label wins
    /// when both own, and `guard` / `card-chain` are the router's own words.
    #[tokio::test]
    async fn routing_label_is_guard_or_render_owned_with_its_label() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let (app, _platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
        let cards = app.cards_handle();
        let waits = app.waits_handle();

        let rows: &[(Claim, Option<CardState>, Option<&str>)] = &[
            (Claim::None, None, None),
            // The pending claim alone owns no card chain: new Turn, never a
            // Supplement.
            (Claim::Inbound, None, None),
            (Claim::Inbound, Some(CardState::Waiting), None),
            (Claim::Inbound, Some(CardState::Done), None),
            (Claim::Guard, None, Some("guard")),
            (Claim::Guard, Some(CardState::Done), Some("guard")),
            (Claim::None, Some(CardState::Streaming), Some("card-chain")),
            // The product: a pending message beside a render-owned card still
            // sees the card's ownership for routing.
            (Claim::Inbound, Some(CardState::Streaming), Some("card-chain")),
            // Both halves own: the guard's label wins.
            (Claim::Guard, Some(CardState::Streaming), Some("guard")),
            // A yielded or spent card is not render-owned.
            (Claim::None, Some(CardState::Waiting), None),
            (Claim::None, Some(CardState::Restarted), None),
            (Claim::None, Some(CardState::Done), None),
        ];

        for (claim, state, expected) in rows {
            set_claim(&app, *claim).await;
            set_card(&cards, state.clone()).await;
            let ownership = CardOwnership::read(&cards, &waits, SID).await;
            assert_eq!(
                ownership.routing_label(),
                *expected,
                "routing for {claim:?} on {state:?}"
            );
        }
    }

    /// The read carries the identities the remaining rules compare: the card's
    /// message id, its Turn anchor and its chain identity — all absent when
    /// the Session has no card.
    #[tokio::test]
    async fn read_carries_the_cards_identities() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let (app, _platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
        let cards = app.cards_handle();
        let waits = app.waits_handle();

        let anchor = turn_anchor(7);
        Turn::seed_card(&cards, SID, Some("om_live")).await;
        Turn::set_card_state(&cards, SID, CardState::Streaming).await;
        Turn::set_turn_anchor(&cards, SID, &anchor).await;
        let chain = Turn::chain_id(&cards, SID).await.expect("seeded card");

        let ownership = CardOwnership::read(&cards, &waits, SID).await;
        assert_eq!(ownership.card_message_id(), Some("om_live"));
        assert_eq!(ownership.turn_anchor(), Some(&anchor));
        assert_eq!(ownership.chain_id(), Some(chain));

        Turn::drop_card(&cards, SID).await;
        let absent = CardOwnership::read(&cards, &waits, SID).await;
        assert_eq!(absent.card_message_id(), None);
        assert_eq!(absent.turn_anchor(), None);
        assert_eq!(absent.chain_id(), None);
    }

    /// The reap's claim (ADR-0063): a guard or a pending-inbound message
    /// claims the Session — the message about to land included, unlike routing
    /// (the #428 strand) — and the claim is independent of the card class.
    #[tokio::test]
    async fn reap_claim_is_guard_or_inbound() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let (app, _platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
        let cards = app.cards_handle();
        let waits = app.waits_handle();

        let rows: &[(Claim, bool)] = &[(Claim::None, false), (Claim::Inbound, true), (Claim::Guard, true)];
        for (claim, expected) in rows {
            set_claim(&app, *claim).await;
            set_card(&cards, None).await;
            assert_eq!(
                CardOwnership::read(&cards, &waits, SID).await.reap_claim(),
                *expected,
                "the reap claim for {claim:?} with no card"
            );
            // The claim half reads the same beside any card class: a yielded
            // card this process no longer renders still claims the reap.
            set_card(&cards, Some(CardState::Waiting)).await;
            assert_eq!(
                CardOwnership::read(&cards, &waits, SID).await.reap_claim(),
                *expected,
                "the reap claim for {claim:?} beside a yielded card"
            );
        }
    }

    /// A stale inbound claim reads as absent and its entry is dropped by the
    /// read — the TTL expiry is invisible (the claim a caller sees is "none",
    /// never an error), and uniform (every verdict read applies it).
    #[tokio::test]
    async fn an_expired_inbound_claim_reads_as_absent_and_is_dropped() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let (app, _platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
        let cards = app.cards_handle();
        let waits = app.waits_handle();

        // The claim TTL is 120s (`INBOUND_CLAIM_TTL`); age this entry past it.
        waits.inbound.lock().await.insert(
            SID.to_string(),
            std::time::Instant::now() - std::time::Duration::from_secs(121),
        );
        let ownership = CardOwnership::read(&cards, &waits, SID).await;
        assert_eq!(ownership.claim(), Claim::None);
        assert!(
            !waits.inbound.lock().await.contains_key(SID),
            "the expired entry is dropped by the read"
        );
    }
}
