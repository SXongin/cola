//! The card's Interaction Block surface (ADR-0038): the inline Permission and
//! Question blocks a live card carries, the one resolution path, and the
//! vanished sweep that ends a block whose request disappeared.
//!
//! These operations were moved verbatim out of the Turn module so the
//! accumulator's interaction surface has one home; the card session map still
//! lives in [`super::state`] and every call takes the same narrow
//! [`CardsHandle`] the rest of the Turn layer uses. Behaviour is unchanged.

use std::collections::HashSet;

use crate::bridge::handles::CardsHandle;
use crate::opencode;

use super::{InlineResidue, Turn, TurnPinSource, state};

impl Turn {
    /// Whether ANY card session still carries a block for `request_id` (the
    /// snapshot-claimability probe, ADR-0028).
    pub(crate) async fn has_interaction(cards: &CardsHandle, request_id: &str) -> bool {
        cards
            .cards
            .lock()
            .await
            .values()
            .any(|c| c.acc.interaction(request_id).is_some())
    }

    /// Whether `session_id`'s card carries a block (live or tombstone) for
    /// `request_id` — the re-host's "nothing to move" probe (ADR-0038, rule 1).
    pub(crate) async fn has_interaction_in(cards: &CardsHandle, session_id: &str, request_id: &str) -> bool {
        cards
            .cards
            .lock()
            .await
            .get(session_id)
            .is_some_and(|c| c.acc.interaction(request_id).is_some())
    }

    /// The Instant Reminder source facts of a cola Turn's live card (ADR-0043).
    /// `None` when no cola turn registered one — an external turn, or a request
    /// pending across a restart.
    pub(crate) async fn pin_source(cards: &CardsHandle, session_id: &str) -> Option<TurnPinSource> {
        let live = cards.cards.lock().await;
        let card = live.get(session_id)?;
        let generation = card.acc.turn_generation?;
        let requester_open_id = card.acc.requester_open_id.clone()?;
        if requester_open_id.is_empty() {
            return None;
        }
        Some(TurnPinSource {
            generation,
            requester_open_id,
            is_group: card.acc.is_group,
        })
    }

    /// Add a permission's inline block to `session_id`'s card. Returns false
    /// when the card already carries the request, or the card session is gone.
    pub(crate) async fn add_permission(
        cards: &CardsHandle,
        session_id: &str,
        p: &opencode::types::PermissionRequest,
        directory: &str,
    ) -> bool {
        let block = state::InteractionBlock::Permission(state::PendingPermission {
            session_id: p.session_id.clone().unwrap_or_default(),
            request_id: p.request_id.clone(),
            body: crate::bridge::request::kind::describe_permission(p),
            target: crate::bridge::request::kind::permission_target(p),
            directory: directory.to_string(),
        });
        match cards.cards.lock().await.get_mut(session_id) {
            Some(card) => card.acc.add_interaction(block),
            None => false,
        }
    }

    /// Add a question's inline block to `session_id`'s card, carrying the
    /// display state the flow restored for it (ADR-0038, rule 1). Returns false
    /// when the card already carries the request, or the card session is gone.
    pub(crate) async fn add_question(
        cards: &CardsHandle,
        session_id: &str,
        q: &opencode::types::QuestionRequest,
        directory: &str,
        answers: &[Option<Vec<String>>],
        done: &[bool],
    ) -> bool {
        let block = state::InteractionBlock::Question(state::PendingQuestion {
            request_id: q.id.clone(),
            session_id: q.session_id.clone(),
            questions: q.questions.clone(),
            directory: directory.to_string(),
            answers: answers.to_vec(),
            done: done.to_vec(),
        });
        match cards.cards.lock().await.get_mut(session_id) {
            Some(card) => card.acc.add_interaction(block),
            None => false,
        }
    }

    /// Replace a question block's display state (the live 已选/✅ markers) in
    /// place. Returns false when the card has no question block for the request
    /// or the card session is gone.
    pub(crate) async fn update_question_state(
        cards: &CardsHandle,
        session_id: &str,
        request_id: &str,
        answers: &[Option<Vec<String>>],
        done: &[bool],
    ) -> bool {
        match cards.cards.lock().await.get_mut(session_id) {
            Some(card) => card.acc.update_question_state(request_id, answers, done),
            None => false,
        }
    }

    /// Resolve every live permission block whose request vanished into its
    /// Interaction Receipt — `dead` names the owning sessions whose run is
    /// over, which get the interrupted line; every other session keeps the
    /// neutral "another client" one. An item owned by a directory whose list
    /// failed, or that cola itself is answering, stays live (#130, #144).
    /// Returns the affected session ids — the sweep repaints each affected card
    /// so the receipt lands within one poll (ADR-0038).
    pub(crate) async fn resolve_vanished_permissions(
        cards: &CardsHandle,
        pending: &HashSet<String>,
        failed_dirs: &HashSet<String>,
        cola_claimed: &HashSet<String>,
        dead: &HashSet<String>,
    ) -> Vec<String> {
        Self::resolve_vanished(cards, pending, failed_dirs, cola_claimed, dead, |block| {
            state::block_of_kind(crate::bridge::snapshot_claims::ClaimKind::Permission, block)
        })
        .await
    }

    /// [`Self::resolve_vanished_permissions`] for the question kind.
    pub(crate) async fn resolve_vanished_questions(
        cards: &CardsHandle,
        pending: &HashSet<String>,
        failed_dirs: &HashSet<String>,
        cola_claimed: &HashSet<String>,
        dead: &HashSet<String>,
    ) -> Vec<String> {
        Self::resolve_vanished(cards, pending, failed_dirs, cola_claimed, dead, |block| {
            state::block_of_kind(crate::bridge::snapshot_claims::ClaimKind::Question, block)
        })
        .await
    }

    /// The owning sessions (with their directory) of the live blocks a kind's
    /// vanished pass would resolve — the sweep classifies these before choosing
    /// the receipt line. One predicate with the resolver below, so the
    /// classifier and the resolution can never disagree.
    pub(crate) async fn vanished_block_sessions(
        cards: &CardsHandle,
        kind: crate::bridge::snapshot_claims::ClaimKind,
        pending: &HashSet<String>,
        failed_dirs: &HashSet<String>,
        cola_claimed: &HashSet<String>,
    ) -> Vec<(String, String)> {
        let own = move |block: &state::InteractionBlock| state::block_of_kind(kind, block);
        let live = cards.cards.lock().await;
        let mut sessions = Vec::new();
        for card in live.values() {
            sessions.extend(state::vanished_block_sessions(
                &card.acc,
                pending,
                failed_dirs,
                cola_claimed,
                own,
            ));
        }
        sessions
    }

    /// The shared sweep body: resolve every live block `own` accepts whose
    /// request vanished, over every card session, picking the interrupted line
    /// for a block whose owning session is in `dead`. Returns the affected
    /// session ids.
    async fn resolve_vanished(
        cards: &CardsHandle,
        pending: &HashSet<String>,
        failed_dirs: &HashSet<String>,
        cola_claimed: &HashSet<String>,
        dead: &HashSet<String>,
        own: impl Fn(&state::InteractionBlock) -> bool,
    ) -> Vec<String> {
        let mut live = cards.cards.lock().await;
        let mut affected = Vec::new();
        for (session_id, card) in live.iter_mut() {
            let line = |block: &state::InteractionBlock| {
                crate::bridge::request::delivery::vanished_receipt(
                    block.session_id(),
                    &block.receipt_target(),
                    dead,
                )
            };
            if state::resolve_vanished_blocks(&mut card.acc, pending, failed_dirs, cola_claimed, &own, line)
                > 0
            {
                affected.push(session_id.clone());
            }
        }
        affected
    }

    /// Resolve `ids` on `session_id`'s accumulator (ADR-0038, rule 4): each
    /// block becomes a tombstone and its receipt joins the timeline keyed at
    /// the resolution moment; a mode change (`Single`) leaves ONE receipt for
    /// everything it resolved. Returns the post-resolution header
    /// `(title, template)` when a block was actually resolved here, so the
    /// caller can restamp the cards it edits in the click's ack.
    pub(crate) async fn resolve_interactions(
        cards: &CardsHandle,
        session_id: &str,
        ids: &[String],
        residue: &InlineResidue<'_>,
    ) -> Option<(String, &'static str)> {
        let mut live = cards.cards.lock().await;
        let acc = &mut live.get_mut(session_id)?.acc;
        let mut resolved_here = false;
        for id in ids {
            let resolved = match residue {
                InlineResidue::PerBlock(line) => {
                    acc.resolve_interaction(id, |block| line(&block.receipt_target()))
                }
                InlineResidue::Single(_) => acc.dismiss_interaction(id),
            };
            resolved_here |= resolved;
        }
        if let InlineResidue::Single(text) = residue
            && resolved_here
        {
            acc.push_receipt(text);
        }
        resolved_here.then(|| acc.header_title_and_template())
    }

    /// The clicked card's updated JSON, built from a CLONE of the session's
    /// accumulator — a split probe that cannot advance the live `render_from`
    /// from inside a click handler (the flush owns that flow). `None` when the
    /// card needs a split or no accumulator exists.
    pub(crate) async fn ack_card(cards: &CardsHandle, session_id: &str) -> Option<serde_json::Value> {
        cards
            .cards
            .lock()
            .await
            .get_mut(session_id)
            .map(|card| {
                let mut probe = card.acc.clone();
                probe.build_card_with_split()
            })
            .and_then(|(card, full)| (!full).then_some(card))
    }
}
