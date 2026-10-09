//! The Wake continuation and projection surface (ADR-0059, ADR-0071): the
//! card operations Session Sync arms a Wake's continuation with, the
//! projection's successor chain, and the one settle loop a continuation's
//! ending runs through.
//!
//! Moved verbatim out of the Turn module so the Wake/projection interface has
//! one home; the types it owns ([`WakeContinuation`], [`ContinuationLine`],
//! [`ContinuationFacts`], [`ProjectedCard`], [`ProjectedSlice`],
//! [`ArmedTakeover`]) are re-exported from [`super`] so every external
//! `turn::…` path is unchanged. Behaviour is unchanged.

use std::sync::Arc;

use crate::backend::{MessageId, SessionTranscript, TurnAnchor, WakeSource};
use crate::bridge::chain::{ChainRecord, RenderedCursor};
use crate::bridge::handles::{CardsHandle, FlowHandles, TurnHandles};
use crate::bridge::turn::state::StreamAccumulator;

use super::{Disposition, SettleTiming, Turn, WAKE_RECEIPT, ownership, render, settle, state};

/// What Session Sync owes for one read (ADR-0059): the decision carries
/// everything the caller needs to act on it, so the same lock acquisition
/// that found the chain also says which handoff to use — nothing is
/// re-derived under a second look.
pub(crate) enum WakeContinuation {
    /// A card chain exists whose card is an OPEN yielded card (「⏳ 等待后台任务」)
    /// and has unrendered content: resume that card IN PLACE (ADR-0066) — no new
    /// card, no 承接 line. Two resumption shapes reach it (spec #602):
    ///
    /// - the newest placeable Wake is a shell/subagent completion this chain has
    ///   not TAKEN OVER yet — the task-completion handoff: its completion entry
    ///   and the remaining live list land on the card the task lived on, and the
    ///   shared out-of-turn settle loop streams the resumed work into it;
    /// - the chain already handed a Wake over (a 承接 line's split or an earlier
    ///   resume) and further content arrived — a Wake-less tail, or the same
    ///   Wake's still-streaming work. It renders onto this same card, never a
    ///   second continuation: exactly one Card per resumption, the receipt
    ///   pushed once.
    ///
    /// `wake_id` is the completion this resume takes over, when there is one,
    /// marked as such under the delivery's own write lock — so a later tail
    /// past it keeps resuming this card, and the resume is never re-decided. A
    /// Wake-less tail carries `None`. Decided only for the state this delivery
    /// admits ([`ownership::admits_ledger_refresh`]), so a decision and its
    /// write can never disagree about which card resumes.
    ResumeInPlace { wake_id: Option<String> },
    /// A card chain exists: a GENUINE resumption the chain has not handed over
    /// yet (a restart, an interruption, a shell/subagent completion on a card
    /// past its wait) continues it by split (spec #602). Only the content the
    /// chain has not rendered lands on the continuation, and the accumulator's
    /// own anchor scopes the settle decision. `line` is the new card's opening
    /// 承接 line, whose key sits just before the work this continuation will
    /// render (that work is already in the past at poll time, so a key at
    /// cola's "now" would sort the receipt after it — the live order bug).
    ContinueChain { line: ContinuationLine },
    /// A card chain exists but its card is past its wait (terminal or
    /// restart-stamped) and unrendered content remains that no un-handed-over
    /// genuine Wake announced — the RESIDUAL floor (spec #602, ticket #606).
    /// The Backend wrote a Wake-less part after the run already reported idle;
    /// cola renders it honestly on ONE neutrally-labeled continuation card
    /// (「📄 还有更新」), never 「已恢复执行」. `line` carries the neutral
    /// receipt's key and no Wake (`wake: None`), so it writes no 承接 line and
    /// marks nothing as handed over. The chain's own `residual_card_posted`
    /// marker bounds it to one per request, and the same-snapshot ending rule
    /// (#604) makes the whole arm near-unreachable.
    Residual { line: ContinuationLine },
    /// A card chain whose card is past its wait and has ALREADY posted its one
    /// neutral residual card (spec #602, ticket #606): a later Wake-less tail
    /// renders IN PLACE on that same neutral card — no second card, no receipt,
    /// no resumption — keeping the ending the card already carries (ADR-0074:
    /// a terminal card's recorded ending is never rewritten, but the content a
    /// user is owed is never dropped either). The card may have settled to a
    /// terminal state by the time the tail arrives; the tail still lands on it,
    /// rendered against a freshly restored copy of that ending. This is the
    /// "content is never dropped" half of #606, paired with [`Self::Residual`]'s
    /// "at most one such card per request" half.
    RenderResidualInPlace,
    /// No chain in this process and no durable record (a cola restart that
    /// left nothing behind): arm a fresh card, scoped at the newest Wake's own
    /// anchor so the lost card's content is never replayed.
    Fresh { anchor: TurnAnchor },
}

/// The opening receipt line of a continuation card (ADR-0059): the timeline
/// key the line takes — just before the work the continuation renders — and,
/// when the continuation answers a Wake, the Wake whose completion the line
/// already announces, with its own server time (the durable Wake Watermark's
/// value once the line's card sends, ADR-0061), so the merged-path receipt (the
/// render pass's) cannot double it. A [`WakeContinuation::ContinueChain`] line
/// always names its Wake; the neutral residual
/// ([`WakeContinuation::Residual`], spec #602/#606) carries `wake: None` — it
/// announces no resumption — and [`WakeContinuation::ResumeInPlace`] carries no
/// line at all.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ContinuationLine {
    pub(crate) at: i64,
    pub(crate) wake: Option<(String, i64)>,
}

/// What [`Turn::arm_projected_card`] armed and what the caller must confirm
/// after the successor's create lands (spec #561).
pub(crate) struct ProjectedCard {
    /// The successor card to send (create semantics, never outbox-retried).
    pub(crate) card: serde_json::Value,
    /// Whether any content actually entered the successor. `false` means the
    /// cursor covered the whole read: nothing was missed, so the ended
    /// projection keeps today's in-place settle instead of posting an empty
    /// successor, while a waiting projection still sends — the wait's card is
    /// the one its completion Wake resumes in place (#583). The live adoption
    /// sends either way — the run is live and the follow streams what it
    /// produces next (ticket #564).
    pub(crate) rendered: bool,
    /// The armed [`state::CardSession::chain_id`] — the identity
    /// [`Turn::take_over_armed_card`] verifies inside its one cards-map
    /// critical section: a fresh Turn that replaced the armed session
    /// meanwhile owns the chain, so the projection must leave the session and
    /// the record alone (spec #561, review #569).
    pub(crate) chain_id: u64,
    /// The exact staged Rendered Cursor the armed body carries (spec #561,
    /// review #569): the confirmation after the create names this stage, so a
    /// body staged since — or a replaced session — is never advanced by the
    /// projection's late create.
    pub(crate) cursor_stage: state::StagedCursorId,
    /// The stage generation of the Wake Watermark the armed body carries, if
    /// any: the create's drain names it, so a watermark staged since is left
    /// untouched.
    pub(crate) watermark_stage: Option<u64>,
    /// Whether MORE content remains after this body (spec #561, review #569):
    /// the delta overflowed one card, so the caller continues the chain with
    /// [`Turn::next_projected_slice`] until a slice fits.
    pub(crate) full: bool,
}

/// One continuation slice of a projection's successor chain (spec #561, review
/// #569): a bounded card built by the normal splitter, with the stage
/// identities its create must confirm.
pub(crate) struct ProjectedSlice {
    pub(crate) card: serde_json::Value,
    pub(crate) cursor_stage: state::StagedCursorId,
    pub(crate) watermark_stage: Option<u64>,
    /// More content remains after this slice.
    pub(crate) full: bool,
}

/// The outcome of [`Turn::take_over_armed_card`] (spec #561, review #569): the
/// armed session was still current — the successor attached and the record
/// re-pointed inside one cards-map critical section — or a fresh Turn had
/// replaced it, in which case nothing was touched.
#[derive(Debug)]
pub(crate) enum ArmedTakeover {
    /// The session is no longer the armed one: the caller must not touch the
    /// session or the record, and collects only its late card.
    Lost,
    /// The takeover landed. The predecessor record is handed back when the
    /// record named a different card (the caller's deferred collect target).
    Took(Option<Box<ChainRecord>>),
}

/// The destination and identity a Wake continuation card is armed with:
/// where it replies, its subtitle, the session's work directory (the Turn
/// Footer), the `/think` variant captured at arm time (ADR-0019) and the
/// Chat a top-level card's later continuations fall back to. Grouped so the
/// arming call stays readable.
pub(crate) struct ContinuationFacts<'a> {
    /// The Feishu message the card replies to. `None` lets the caller send at
    /// the chat top level (a restart leaves no reply target to recover); the
    /// split path needs one and resolves it before choosing that branch.
    pub(crate) reply_to: Option<&'a str>,
    /// The Chat the card belongs to: the top-level target a size split of
    /// this card continues into when `reply_to` is `None`.
    pub(crate) chat_id: &'a str,
    pub(crate) subtitle: &'a str,
    pub(crate) directory: &'a str,
    pub(crate) variant: Option<String>,
}

/// The Wake-continuation interface (ADR-0059): the decision Session Sync asks,
/// the card operations it arms the continuation with, and the out-of-turn loop
/// its ending runs through. The accumulator and the chain's identity stay
/// private to the Turn module; Session Sync only learns what is owed and which
/// anchor scopes it.
impl Turn {
    /// The turn anchor of the session's armed renderer, if one is armed: the
    /// renderer identity both external arming paths compare their own turn's
    /// anchor against. The full anchor is the identity — message id together
    /// with server time — because two user messages can share a millisecond,
    /// so the time alone cannot tell two turns apart.
    pub(crate) async fn armed_turn_anchor(cards: &CardsHandle, session_id: &str) -> Option<TurnAnchor> {
        cards
            .cards
            .lock()
            .await
            .get(session_id)
            .and_then(|c| c.acc.turn_anchor.clone())
    }

    /// Arm a projection's successor card (spec #561): a fresh accumulator
    /// seeded from the chain's Rendered Cursor renders the transcript's delta
    /// once — the cut tail with its markdown lead, the live set resolved by
    /// identity — takes the ledger and work context, applies `ending` when the
    /// run is over (the ended-while-down projection, ticket #563) and is
    /// inserted as the session's live card, ready for the caller's create.
    /// `None` when the session already has a card (a chain appeared meanwhile)
    /// — the caller must send nothing.
    ///
    /// With `ending: None` the successor follows a still-live run (ticket
    /// #564): it wears a working header from its first send — never an initial
    /// 「思考中」, the run it continues is not starting — and the caller arms
    /// the external follow on this same accumulator.
    ///
    /// The first send itself stays with the caller (the reap's apply): the
    /// successor replies to the original Turn anchor, the recorded card, or
    /// the chain's top-level chat, and its create is never outbox-retried.
    /// The accumulator's own reply target is the Turn anchor, so a later size
    /// split of the successor continues from the user's message. The returned
    /// [`ProjectedCard`] carries what the caller confirms after the create:
    /// whether any content actually rendered (nothing missed keeps today's
    /// settle path), and whether a live-set call settled (the old card then
    /// drops the running panels the successor resolved, ADR-0068's
    /// generalized collect).
    #[allow(clippy::too_many_arguments)] // the successor's whole arming fixture
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn arm_projected_card(
        cards: &CardsHandle,
        backend: &Arc<dyn crate::backend::Backend>,
        session_id: &str,
        anchor: &TurnAnchor,
        cursor: &RenderedCursor,
        seed: &state::CursorSeed,
        gap: Option<&crate::bridge::chain::PendingGap>,
        transcript: &SessionTranscript,
        ending: Option<&Disposition>,
        title: &str,
        directory: &str,
        fallback_chat: Option<&str>,
        variant: Option<String>,
    ) -> Option<ProjectedCard> {
        let work_context = StreamAccumulator::capture_work_context(directory).await;
        // Build the successor's accumulator first, OUTSIDE the cards lock: the
        // completion entries it owes are planned on it and their output tails
        // read before the lock is taken (spec #593; the reads are network).
        let mut acc = StreamAccumulator::new(title);
        Self::seed_wake_floor(cards, session_id, &mut acc);
        acc.turn_anchor = Some(anchor.clone());
        acc.set_session(session_id);
        // The chain's reply target (issue #580) is the target the successor's
        // create actually lands on — the durable reply target, or the recorded
        // card a refused target falls back to. `take_over_armed_card` sets it
        // under the takeover's own critical section; an OpenCode message id is
        // never a deliverable Feishu target, so the anchor is not one.
        acc.set_variant(variant);
        // The successor continues a chain: it carries no question to re-ask,
        // so an Error ending never offers Retry (ADR-0059).
        acc.mark_wake_continuation();
        acc.apply_work_context(work_context);
        acc.seed_projection(cursor, seed.clone());
        // A pending orphan gap renders first (spec #561, review #569): its
        // content was never on a card, while the chain's cursor has already
        // advanced past the delivered content that follows it. The seed then
        // governs the successor's own window — the delivered content is never
        // re-rendered.
        if let Some(gap) = gap {
            acc.set_pending_gap(gap.clone());
        }
        let mut plans = {
            let live = cards.cards.lock().await;
            if live.contains_key(session_id) {
                return None;
            }
            render::plan_ledger_entries(&acc, transcript, Some(anchor))
        };
        render::read_planned_outputs(backend, &mut plans).await;
        let mut live = cards.cards.lock().await;
        if live.contains_key(session_id) {
            return None;
        }
        let rendered = render::render_turn_parts(&mut acc, transcript);
        render::apply_ledger_read(
            &mut acc,
            transcript,
            &std::collections::HashMap::new(),
            chrono::Utc::now().timestamp_millis(),
            state::LedgerCadence::Minute,
            plans,
        );
        match ending {
            Some(ending) => acc.apply_ending(ending),
            None => {
                // A live adoption follows a run already in flight: the
                // successor is a working card from its first send, never an
                // initial 「思考中」.
                acc.mark_adopted_live();
            }
        }
        // The successor's body goes through the SAME splitter the flush uses
        // (spec #561, review #569): an oversized missed delta must become a
        // bounded chain of cards, never one over-limit create that fails and
        // loses the tail. A finalized slice wears 「部分完成」 even when the
        // ending is terminal; the final slice wears the ending.
        let built = acc.build_card_with_info();
        // Stage the body's cursor exactly like a flush does: only the
        // confirmed create drains it into the Chain Record. The stage
        // identities travel on the [`ProjectedCard`], so the confirmation can
        // name the exact body the create carried (spec #561, review #569).
        let cursor_stage = state::StagedCursorId {
            id: acc.stage_cursor(None, built.cursor.clone(), built.gap.clone()),
            awaiting_seq: None,
        };
        let watermark_stage = acc.pending_watermark_id();
        let mut session = state::CardSession::new(acc, None);
        // A finalized first slice is not the chain's live card: the caller
        // continues the chain on a new card.
        session.card_is_live = !built.full;
        session.fallback_chat = fallback_chat.map(str::to_string);
        // The caller re-checks this identity after its awaited create: a fresh
        // Turn that replaced the armed session meanwhile owns the chain (spec
        // #561, review #569).
        let chain_id = session.chain_id();
        live.insert(session_id.to_string(), session);
        Some(ProjectedCard {
            card: built.card,
            rendered,
            chain_id,
            cursor_stage,
            watermark_stage,
            full: built.full,
        })
    }

    /// Arm an external renderer's card: build the turn's accumulator, attach
    /// the work context, push the anchor text (the message preview / snapshot
    /// identity) just before the turn's server-time anchor so the reply's parts
    /// always insert below it, and insert the card session the render loop
    /// streams into. `anchor` is the external message's identity together with
    /// its server time, one fact; `variant` is the session's `/think` override
    /// captured at ARM time (ADR-0019).
    #[allow(clippy::too_many_arguments)] // the card's whole arming fixture
    pub(crate) async fn arm_external_render(
        cards: &CardsHandle,
        session_id: &str,
        card_id: &str,
        anchor: &TurnAnchor,
        subtitle: &str,
        session_dir: &str,
        variant: Option<String>,
        anchor_text: Option<&str>,
    ) {
        let mut acc = state::StreamAccumulator::new(subtitle);
        Self::seed_wake_floor(cards, session_id, &mut acc);
        // The external message's anchor — identity plus server time — is the
        // turn's anchor: header date, turn filter and renderer replacement
        // guard all read it, and cola's clock is never part of the card
        // (#183, #190).
        acc.turn_anchor = Some(anchor.clone());
        acc.set_session(session_id);
        acc.set_reply_target(Some(card_id.to_string()));
        acc.attach_work_context(session_dir).await;
        acc.set_variant(variant);
        if let Some(text) = anchor_text.filter(|text| !text.is_empty()) {
            acc.push_text_at(Some(anchor.created_ms - 1), text);
        }
        cards.cards.lock().await.insert(
            session_id.to_string(),
            state::CardSession::new(acc, Some(card_id.to_string())),
        );
        // An adopted run's card is the session's live card: take the chain over
        // (the anchor is known here, so the record needs no transcript probe,
        // and the arm carries the directory its reads route under). A different
        // previously recorded card was orphaned by this arm and is collected
        // as taken over.
        Self::take_over_card(cards, session_id, card_id, Some(session_dir)).await;
    }

    /// What Session Sync owes for this read: `None` when the chain has
    /// nothing unrendered, `Some([`WakeContinuation`])` otherwise. The caller
    /// owns the scope predicates: the Session must be the thread's Active
    /// Session, its newest user message must be a Cola-Authored Message, and
    /// no live Turn/follow/renderer may own the card
    /// ([`CardOwnership::routing_label`]).
    ///
    /// The decision is a content diff over the Session Transcript (ADR-0059),
    /// never a terminal step:
    ///
    /// - **A card chain exists.** The only question is whether the chain
    ///   missed anything: rendering the Turn into the chain's own rendered
    ///   state must produce a part — a Wake's resumed work, or content that
    ///   landed after the card was finalized. Nothing new renders -> nothing
    ///   is owed, which is also what keeps a rendered Wake from being
    ///   re-posted on every poll. What it owes then obeys spec #602: an open
    ///   yielded card resumes in place ([`WakeContinuation::ResumeInPlace`],
    ///   ADR-0066) — for a task-completion handoff, or for a Wake-less tail on a
    ///   chain that already took a Wake over — and only a GENUINE resumption the
    ///   chain has not handed over yet (a shell/subagent completion past its
    ///   wait, a restart, an interrupt) continues the chain by split. A
    ///   Wake-less content diff on an OPEN yielded card resumes in place; on a
    ///   card PAST its wait it is the RESIDUAL floor ([`WakeContinuation::Residual`],
    ///   spec #602/#606): it renders on one neutral card, never a 「已恢复执行」
    ///   receipt, and never a second one.
    /// - **No chain in this process (a cola restart).** A durable Chain Record
    ///   means the projection — or, cursorless, the reap's one-release
    ///   fallback — owns the chain's Wake, so the Fresh path owes nothing
    ///   (spec #561, ticket #566). Only a recordless post reaches the Fresh
    ///   gate: the newest placeable Wake's own work may be rendered, scoped at
    ///   the Wake's anchor — the whole Turn is never replayed. A Wake-less read
    ///   owes nothing.
    pub(crate) async fn wake_continuation(
        cards: &CardsHandle,
        session_id: &str,
        transcript: &SessionTranscript,
        turn_anchor: &TurnAnchor,
    ) -> Option<WakeContinuation> {
        let newest_wake = transcript
            .wakes
            .iter()
            .filter(|wake| wake.created_ms.is_some())
            .max_by_key(|wake| wake.created_ms);
        // The diff is a sync read of the accumulator's dedup state, so the
        // cards lock is held only for its scan — no clone of the chain (whose
        // rendered content can be a card's worth) is built to run it off-lock.
        {
            let live = cards.cards.lock().await;
            if let Some(card) = live.get(session_id) {
                // The probe judges the chain's OWN rendered scope, not the
                // newest user message's: a restart continuation is anchored at
                // the Wake (`arm_wake_continuation`), and the pre-Wake content
                // the lost card showed is outside that scope. Judging it by the
                // user anchor read that content as unrendered on every pass and
                // looped a fresh 承接 card each Sync tick (live 2026-09-30). A
                // same-life card's anchor is the user message's, so the common
                // path is unchanged.
                let anchor = card.acc.turn_anchor.as_ref().unwrap_or(turn_anchor);
                if !render::renders_new_content(&card.acc, transcript, anchor) {
                    return None;
                }
                // An OPEN yielded card resumes IN PLACE (ADR-0066), so no second
                // card ever opens on it (spec #602). Precedence matters:
                //
                // 1. The newest placeable Wake is a shell/subagent completion
                //    this chain has not TAKEN OVER yet — the task-completion
                //    handoff. One card per request: the entry, the resumed work
                //    and the ending all stay on the card the task lived on. The
                //    gate reads the HANDOFF, not the announcement: a yielded
                //    card's ledger refresh places the completion entry while the
                //    Wake's work is still unrendered (live 2026-10-01), and that
                //    entry must not masquerade as a handoff.
                if ownership::admits_ledger_refresh(card)
                    && let Some(wake) = newest_wake
                    && matches!(wake.source, WakeSource::Shell | WakeSource::Subagent)
                    && !card.acc.has_handed_over_wake(wake.id.as_str())
                {
                    return Some(WakeContinuation::ResumeInPlace {
                        wake_id: Some(wake.id.to_string()),
                    });
                }
                // 2. A chain continuation opens a NEW card ONLY for a genuine
                //    resumption this chain has not handed over yet: a restart or
                //    an interruption (a shell/subagent completion on a card past
                //    its wait reached the same branch). Key the 承接 receipt just
                //    before the work the continuation will render: the Wake's own
                //    server time (that work's parts are already in the past at
                //    poll time, so they sort after this key). The covered Wake is
                //    marked too, so its completion is announced by this line,
                //    never doubled by the merged-path entry — and, once marked, a
                //    later tail resumes the continuation in place instead of
                //    splitting again.
                if let Some(wake) = newest_wake
                    && wake.source.is_genuine_resumption()
                    && !card.acc.has_handed_over_wake(wake.id.as_str())
                    && let Some(anchor) = wake.anchor()
                {
                    return Some(WakeContinuation::ContinueChain {
                        line: ContinuationLine {
                            at: anchor.created_ms.saturating_sub(1),
                            wake: Some((anchor.message_id.to_string(), anchor.created_ms)),
                        },
                    });
                }
                // 3. Any other OPEN yielded card with unrendered content resumes
                //    IN PLACE: a Wake-less tail, or the same Wake's still-
                //    streaming work past a handoff. It renders onto the SAME
                //    card — the restart double-Card fix (spec #602,
                //    `ses_ee3f19fc4ffefrgIFcTk6zV8yH`) — so it never opens a
                //    second continuation and never writes a receipt for work no
                //    Wake announced. The newest Wake, when there is one, is
                //    already handed over, so re-marking it is a no-op; a Wake-less
                //    tail (`None`) marks nothing. This is also why a Wake-less
                //    tail is not dropped: an open yielded card renders it.
                if ownership::admits_ledger_refresh(card) {
                    return Some(WakeContinuation::ResumeInPlace {
                        wake_id: newest_wake.map(|wake| wake.id.to_string()),
                    });
                }
                // 4. A card past its wait (terminal or restart-stamped) with
                //    unrendered content and no un-handed-over genuine Wake is
                //    the RESIDUAL floor (spec #602, ticket #606): the Backend
                //    wrote a Wake-less part after the run reported idle. The
                //    same-snapshot rule (#604) makes it near-unreachable; as a
                //    floor it renders honestly on ONE neutral continuation card,
                //    never a 「已恢复执行」 receipt, and the content is never
                //    dropped. The chain's own marker bounds it to one per
                //    request — once the floor has fired, later late content
                //    renders IN PLACE on that one neutral card (a second neutral
                //    card would break one-card-per-request; dropping the content
                //    would break "never dropped").
                if !card.acc.residual_card_posted() {
                    return Some(WakeContinuation::Residual {
                        line: ContinuationLine {
                            // Key the neutral receipt just before the missed
                            // work: the chain's own anchor precedes everything
                            // the card renders, so the receipt sorts to the top
                            // of the continuation's live slice (the same
                            // live-order rule the Wake's 承接 line follows).
                            at: anchor.created_ms.saturating_sub(1),
                            wake: None,
                        },
                    });
                }
                return Some(WakeContinuation::RenderResidualInPlace);
            }
        }
        // No chain in this process (a cola restart). The Fresh gate is the
        // Chain Record module's second decision entry (ADR-0069): it shares
        // this store's durable facts — the Chain Record itself and the Wake
        // Watermark — and the Keep / NoDecision vocabulary, and owns the rules
        // the read facts are judged by. The corroboration the gate cannot
        // decide — the in-process inbound claim and the anchor re-read — stays
        // with `render_wake_continuation`'s caller.
        // Only a GENUINE resumption may open a continuation Card (spec #602):
        // a Wake whose source this build cannot classify (an `Other(_)` marker,
        // or `Unknown` on a payload that named none) is not a resumption, so it
        // reads as no Wake here — the Fresh gate then decides NoDecision/Keep
        // and opens nothing. The live-card path gates on the same predicate
        // (#605); this is the Fresh-path half of one rule.
        let wake = newest_wake
            .filter(|wake| wake.source.is_genuine_resumption())
            .and_then(|wake| wake.anchor());
        // A Wake older than the newest user message is STALE: the conversation
        // has moved past it — a later cola life already saw or superseded it —
        // and re-posting it after a restart would replay every turn that
        // followed (the live 102k-char replay). The genuine restart case (the
        // Wake's run is still pending, or it finished while cola was down) has
        // no newer user message and still posts. Only the Fresh path needs
        // this: a chain continuation renders just what the chain missed, so a
        // stale Wake can never replay history through it, and the content-diff
        // fallback (which also fires with a chain) stays untouched.
        let stale = wake.as_ref().is_some_and(|anchor| {
            transcript
                .newest_user()
                .and_then(|message| message.time)
                .is_some_and(|time| time.created > anchor.created_ms)
        });
        let mut reads = crate::bridge::chain::FreshReads {
            announced: cards.chains.announced(session_id),
            // A durable record owns this chain's Wake: the projection (or, for
            // a cursorless record, the reap's fallback) supersedes the Fresh
            // post (spec #561, ticket #566). Only a recordless post survives.
            recorded: cards.chains.recorded(session_id),
            wake,
            stale,
            renders: false,
        };
        // The Wake's own work, scoped at its anchor, against an empty
        // accumulator: a restart never replays the whole Turn. The probe is a
        // full transcript scan, so it runs only when the plan warrants it — an
        // already-announced or stale Wake owes nothing regardless.
        if reads.needs_render_probe() {
            let probe = StreamAccumulator::new("");
            reads.renders = reads
                .wake
                .as_ref()
                .is_some_and(|anchor| render::renders_new_content(&probe, transcript, anchor));
        }
        match crate::bridge::chain::fresh(&reads) {
            crate::bridge::chain::FreshDisposition::Announce(anchor) => {
                Some(WakeContinuation::Fresh { anchor })
            }
            crate::bridge::chain::FreshDisposition::Keep
            | crate::bridge::chain::FreshDisposition::NoDecision => None,
        }
    }

    /// The submitted user message's cola id on `session_id`'s card (ADR-0026),
    /// when the card carries one — the id the unreceived watch's anchor
    /// capture matches on (`capture_turn_anchor`). `None` when the session has
    /// no card, or the card never carried a submitted message (an external
    /// render's, a Wake continuation's): the watch has nothing to look for.
    pub(crate) async fn submitted_message_id(cards: &CardsHandle, session_id: &str) -> Option<String> {
        cards
            .cards
            .lock()
            .await
            .get(session_id)
            .and_then(|card| card.acc.cola_message_id().map(str::to_string))
    }

    /// Arm a FRESH Wake continuation card: the path with neither a chain in
    /// this process nor a durable record (a cola restart that left nothing
    /// behind), so there is nothing to hand over. The new accumulator renders
    /// only the Wake's own work — scoped at
    /// `anchor` — opens with the 承接 line, carries the session's work context,
    /// answers to `facts.reply_to` and is marked a continuation (it offers no
    /// Retry). Returns the card to send; the caller attaches its id with
    /// [`Self::take_over_card`]. `None` when the session has a card after
    /// all — the decision saw none, so a chain appeared meanwhile and owns the
    /// session now; the caller must send nothing and the next poll re-decides.
    pub(crate) async fn arm_wake_continuation(
        cards: &CardsHandle,
        session_id: &str,
        anchor: &TurnAnchor,
        facts: ContinuationFacts<'_>,
    ) -> Option<serde_json::Value> {
        let work_context = StreamAccumulator::capture_work_context(facts.directory).await;
        let mut live = cards.cards.lock().await;
        if live.contains_key(session_id) {
            return None;
        }
        let mut acc = StreamAccumulator::new(facts.subtitle);
        Self::seed_wake_floor(cards, session_id, &mut acc);
        acc.turn_anchor = Some(anchor.clone());
        acc.set_session(session_id);
        acc.set_reply_target(facts.reply_to.map(str::to_string));
        acc.set_variant(facts.variant);
        acc.mark_wake_continuation();
        // The 承接 line announces this Wake's completion and hands its work to
        // this fresh card: mark both, so the merged-path entry never doubles
        // the line when the work renders, and a later tail past the Wake still
        // splits (ADR-0059) instead of resuming this card in place.
        acc.announce_wake(anchor.message_id.as_str(), anchor.created_ms);
        acc.hand_over_wake(anchor.message_id.as_str());
        acc.apply_work_context(work_context);
        // The 承接 line is keyed just before the Wake's own work so the
        // resumed parts — whose server times are at or after the anchor —
        // always insert below it.
        acc.push_text_at(Some(anchor.created_ms.saturating_sub(1)), WAKE_RECEIPT);
        let card = {
            // The initial send is a probe: building it must not advance the
            // live card's render boundary.
            let mut probe = acc.clone();
            probe.build_card_with_split().0
        };
        let mut session = state::CardSession::new(acc, None);
        // A top-level card must be able to continue top-level when its run
        // overflows one card (there is no reply target to hand over to).
        session.fallback_chat = Some(facts.chat_id.to_string());
        live.insert(session_id.to_string(), session);
        Some(card)
    }

    /// Run the Wake continuation's out-of-turn settle loop (ADR-0059) on the
    /// caller's own task: the shared out-of-turn loop under the
    /// continuation's chain identity, then its ending applied through the one
    /// shared, ownership-checked application
    /// ([`ownership::Ticket::apply_ending_if_owned`], #539). Returns the ending
    /// disposition the loop
    /// reached and applied together with the identity of the card it landed on,
    /// or `None` when it stopped owning the card (its
    /// accumulator vanished or a successor took it over) and stamped nothing;
    /// the caller owns the announcement and passes that card id so the notice
    /// is bound to the card the ending landed on (spec #602, review finding 6).
    /// A split continuation needs no notice — its
    /// own card send was the notification — while an in-place resume, which
    /// never sends, notifies at the true end (ADR-0066); the caller holds the
    /// notice rules this module has no config for.
    pub(crate) async fn wake_settle_loop(
        flow: &FlowHandles,
        session_id: &str,
        directory: &str,
        anchor: &TurnAnchor,
        chain: u64,
        timing: SettleTiming,
    ) -> Option<(Disposition, String)> {
        let ticket = ownership::Ticket::Chain {
            chain,
            anchor: anchor.clone(),
        };
        let disposition = settle::run(flow, session_id, directory, timing, &ticket).await?;
        // The same post-run gap as the follow's: the loop's last probe may be
        // stale by now, so the ending is applied atomically with a fresh
        // ownership check. Ownership lost is a loop that ended nothing — the
        // same `None` `run` returns. The returned id is the card the ending
        // landed on, so the caller's notice is bound to it.
        let card_id = ticket
            .apply_ending_if_owned(&flow.cards, session_id, &disposition)
            .await?;
        Some((disposition, card_id))
    }

    /// The request's original Turn start on `session_id`'s card, when it
    /// recorded one — the Completion Notice's long-task clock (ADR-0043). The
    /// quiet true end and the in-place resume's ending both measure the whole
    /// run from here, so the resumed work counts toward the threshold.
    pub(crate) async fn turn_started_at(cards: &CardsHandle, session_id: &str) -> Option<std::time::Instant> {
        cards
            .cards
            .lock()
            .await
            .get(session_id)
            .and_then(|card| card.acc.turn_started_at())
    }

    /// Drop an armed continuation whose card never sent: the session must not
    /// stay owned by a phantom card, so the next poll retries. Only the exact
    /// armed anchor is dropped — a newer session that took over meanwhile is
    /// never removed.
    pub(crate) async fn drop_armed_card(cards: &CardsHandle, session_id: &str, anchor: &TurnAnchor) {
        let mut live = cards.cards.lock().await;
        if live
            .get(session_id)
            .is_some_and(|card| card.acc.turn_anchor.as_ref() == Some(anchor))
        {
            live.remove(session_id);
        }
    }

    /// Drop an armed projection's phantom session — its card never sent — only
    /// while it is still `chain_id`'s, never-attached session (spec #561,
    /// review #569). One cards-map critical section: a fresh Turn that replaced
    /// the session takes this same lock to insert its own card, so it can
    /// never be removed here. Returns whether the armed session was dropped.
    pub(crate) async fn drop_armed_session(cards: &CardsHandle, session_id: &str, chain_id: u64) -> bool {
        let mut live = cards.cards.lock().await;
        let is_armed = live
            .get(session_id)
            .is_some_and(|card| card.chain_id() == chain_id && card.card_message_id.is_none());
        if is_armed {
            live.remove(session_id);
        }
        is_armed
    }

    /// Take an armed projection's successor over — verify, attach and re-point
    /// — inside ONE cards-map critical section (spec #561, review #569). A
    /// projection's create is awaited, so a fresh Turn can insert its own
    /// [`state::CardSession`] at any moment; that insert takes this same map
    /// lock, so holding it across the check, the attach and the durable record
    /// write leaves the replacement no gap to interleave in. The chain store's
    /// operations are synchronous (their own std mutex plus a file write) and
    /// never touch the cards map, so cards → chains is the only nesting order;
    /// the session write-lock is deliberately NOT taken here ([`Self::flush_card`]
    /// takes it before the cards lock).
    ///
    /// `chain_id` is the armed session's [`state::CardSession::chain_id`].
    /// `reply_to` is the rung the successor's create actually landed on — a
    /// deliverable reply target, or `None` for the Chat's top level (an
    /// explicit clear; #580/#582). `cursor_stage` is the exact Rendered Cursor
    /// stage the create's body carried: it is confirmed in the SAME critical
    /// section as the re-point (review #569), so the record never names the
    /// successor with the predecessor's frontier. [`ArmedTakeover::Lost`] means
    /// the session was replaced or attached meanwhile: nothing was touched.
    /// Otherwise the successor id is attached, the record re-pointed carrying
    /// the confirmed cursor into the armed accumulator exactly as
    /// [`Self::track_live_card`] does, and the predecessor record handed back
    /// for the caller's deferred collect.
    pub(crate) async fn take_over_armed_card(
        cards: &CardsHandle,
        session_id: &str,
        chain_id: u64,
        card_message_id: &str,
        reply_to: Option<&str>,
        directory: Option<&str>,
        cursor_stage: state::StagedCursorId,
    ) -> ArmedTakeover {
        match Self::attach_and_repoint(
            cards,
            session_id,
            chain_id,
            true,
            card_message_id,
            None,
            reply_to,
            directory,
            cursor_stage,
        )
        .await
        {
            Some(previous) => ArmedTakeover::Took(previous),
            None => ArmedTakeover::Lost,
        }
    }

    /// Attach a projection chain's just-created continuation (spec #561,
    /// review #569): verify the chain is still the armed one, attach the new
    /// card, mark whether it is the chain's LIVE card (the follow streams onto
    /// it), confirm the create's staged Rendered Cursor and re-point the record
    /// — one cards-map critical section, exactly like the armed takeover, so a
    /// fresh Turn that replaced the session is never touched and the record
    /// never names the new card with the previous slice's frontier. The chain
    /// continues (no predecessor collect). `reply_to` is the rung this slice's
    /// create actually landed on (#582) — a later slice may fall to a
    /// different rung than the first, so it rides the same re-point; `None` (a
    /// top-level slice) clears the durable target. Returns `false` when a
    /// fresh Turn owns the session now: the caller collects the late card and
    /// stops the chain.
    #[allow(clippy::too_many_arguments)] // the slice transition's whole fixture
    pub(crate) async fn track_projected_continuation(
        cards: &CardsHandle,
        session_id: &str,
        chain_id: u64,
        card_message_id: &str,
        card_is_live: bool,
        reply_to: Option<&str>,
        directory: Option<&str>,
        cursor_stage: state::StagedCursorId,
    ) -> bool {
        Self::attach_and_repoint(
            cards,
            session_id,
            chain_id,
            false,
            card_message_id,
            Some(card_is_live),
            reply_to,
            directory,
            cursor_stage,
        )
        .await
        .is_some()
    }

    /// The shared projection transition (spec #561, review #569): under ONE
    /// cards-map lock, check the session is `chain_id`'s (and, for the armed
    /// case, never attached), attach `card_message_id`, mark the live/finalized
    /// state when the caller knows it, take the just-confirmed body's staged
    /// Rendered Cursor and re-point the durable record carrying it. `None`
    /// when the session is not ours; otherwise the predecessor record (handed
    /// back only for the armed case — a continuation's predecessor is the slice
    /// the chain just sent).
    #[allow(clippy::too_many_arguments)] // the projection transition's whole fixture
    async fn attach_and_repoint(
        cards: &CardsHandle,
        session_id: &str,
        chain_id: u64,
        require_unattached: bool,
        card_message_id: &str,
        card_is_live: Option<bool>,
        reply_to: Option<&str>,
        directory: Option<&str>,
        cursor_stage: state::StagedCursorId,
    ) -> Option<Option<Box<ChainRecord>>> {
        let mut live = cards.cards.lock().await;
        let card = live.get_mut(session_id)?;
        if card.chain_id() != chain_id || (require_unattached && card.card_message_id.is_some()) {
            return None;
        }
        if let Some(card_is_live) = card_is_live {
            card.card_is_live = card_is_live;
        }
        // The armed successor's landing reply target (#580/#582): the target
        // its create actually reached — the durable reply target, or the
        // recorded card a refused target fell back to — and `None` when every
        // target was refused and the create landed at the Chat's top level.
        // Applied unconditionally so the accumulator and the record write
        // below agree on the rung, and an explicit None CLEARS both: the
        // landing adopts its rung in ONE write, never a second clear (which a
        // crash could lose, leaving a refused target persisted).
        card.acc.set_reply_target(reply_to.map(str::to_string));
        let (message_id, created_ms, context_directory, reply_to) = (
            card.acc.cola_message_id().map(MessageId::new).or_else(|| {
                card.acc
                    .turn_anchor
                    .as_ref()
                    .map(|anchor| anchor.message_id.clone())
            }),
            card.acc.turn_anchor.as_ref().map(|anchor| anchor.created_ms),
            card.acc.directory().map(str::to_string),
            card.acc.reply_to_message_id().map(str::to_string),
        );
        card.card_message_id = Some(card_message_id.to_string());
        // The create landed: take its exact staged Rendered Cursor NOW, inside
        // this critical section (review #569). A confirmation after this lock —
        // however soon after — would leave the record naming the successor card
        // while it still carried the predecessor's frontier; a fresh Turn
        // snapshotting the record in that window seeds the already-delivered
        // tail onto its own card while the collected successor keeps its body:
        // the same text twice.
        let confirmed = card.acc.take_staged_cursor(card_message_id, cursor_stage);
        // The orphan gap this projection rendered is consumed with the
        // re-point; one it could NOT place stays owed and rides onto the new
        // record (spec #561, review #569), so a later read can still deliver
        // it.
        // The gap this projection's create carried (spec #561, review #569):
        // the re-point already carried it onto the new record, and its coverage
        // now consumes the fact (a body that reached the gap's end) or advances
        // it past what the successor card shows — so neither the continuation
        // nor a restart repeats delivered gap content. No coverage at all
        // leaves the durable fact untouched.
        // A card with no Turn message to scope a settle decision with cannot
        // be reaped: attached, but no record — `track_live_card`'s own rule.
        let Some(message_id) = message_id else {
            return Some(None);
        };
        let directory = directory
            .map(str::to_string)
            .filter(|directory| !directory.is_empty())
            .or(context_directory);
        let previous = match &confirmed {
            // The confirmed body's cursor rides the re-point in ONE chains
            // write, so the record is never observable as (successor card,
            // predecessor cursor) — not even between two chains operations —
            // and the landing's reply target rides the same write (#582).
            Some((cursor, gap)) => cards.chains.track_carrying_cursor_landing(
                session_id,
                card_message_id,
                message_id,
                created_ms,
                directory.as_deref(),
                reply_to.as_deref(),
                cursor,
                gap.as_ref()
                    .map(|coverage| (&coverage.frontier, coverage.complete)),
            ),
            // No staged cursor matched the create's stage: nothing was
            // confirmed, so the re-point carries the previous frontier exactly
            // as a plain re-point does — with the landing's reply target.
            None => cards.chains.track_landing(
                session_id,
                card_message_id,
                message_id,
                created_ms,
                directory.as_deref(),
                reply_to.as_deref(),
            ),
        };
        // The projection's take-over CONSUMES the write-ahead intent (spec
        // #561, review #569): the create the mark covered has landed — this
        // re-point is its confirmation — so no later life may treat it as
        // ambiguous. `track_carrying_cursor` builds a fresh record without the
        // mark (a no-op clear); the plain re-point branch must clear it, and
        // the clear is skipped when there is nothing to clear.
        cards.chains.clear_projection_intent(session_id, card_message_id);
        // A re-point within the chain carries the Rendered Cursor (spec #561):
        // seed the accumulator's empty base with the carried fact, so the
        // successor's first body does not clear the chain's frontier.
        if let Some(cursor) = previous.as_ref().and_then(|record| record.cursor.clone()) {
            let card = live
                .get_mut(session_id)
                .expect("the cards map still holds the session");
            card.acc.seed_cursor_if_empty(cursor);
        }
        match previous {
            Some(previous) if previous.card_message_id != card_message_id => Some(Some(Box::new(previous))),
            _ => Some(None),
        }
    }

    /// Build a projection successor chain's next slice (spec #561, review
    /// #569): the remaining content from `render_from`, bounded by the SAME
    /// splitter the flush uses, with its Rendered Cursor staged for the
    /// create's confirmation. `None` when the session no longer carries the
    /// projection's chain (a fresh Turn owns it) — the caller stops.
    pub(crate) async fn next_projected_slice(
        cards: &CardsHandle,
        session_id: &str,
        chain_id: u64,
    ) -> Option<ProjectedSlice> {
        let mut live = cards.cards.lock().await;
        let card = live.get_mut(session_id)?;
        if card.chain_id() != chain_id {
            return None;
        }
        let built = card.acc.build_card_with_info();
        let cursor_stage = state::StagedCursorId {
            id: card
                .acc
                .stage_cursor(None, built.cursor.clone(), built.gap.clone()),
            awaiting_seq: None,
        };
        Some(ProjectedSlice {
            card: built.card,
            cursor_stage,
            watermark_stage: card.acc.pending_watermark_id(),
            full: built.full,
        })
    }

    /// Rebuild a projection's FIRST slice with the flush's fenced fallback
    /// (spec #561, review #569): a definite card-content rejection means the
    /// same payload can never land, so the same body is re-rendered with every
    /// model-markdown element fenced — exactly the flush's `RetryFenced` path —
    /// rewinding the arm's render boundary and staging the new body's cursor.
    /// `None` when the fenced retry was already refused (`Suspended`: the
    /// content can never land) or the chain is gone.
    pub(crate) async fn fenced_projected_slice(
        cards: &CardsHandle,
        session_id: &str,
        chain_id: u64,
    ) -> Option<ProjectedSlice> {
        use crate::bridge::turn::state::CardFallback;
        let mut live = cards.cards.lock().await;
        let card = live.get_mut(session_id)?;
        if card.chain_id() != chain_id {
            return None;
        }
        match card.acc.card_fallback {
            CardFallback::None => card.acc.card_fallback = CardFallback::Fenced,
            CardFallback::Fenced | CardFallback::Suspended => {
                card.acc.card_fallback = CardFallback::Suspended;
                return None;
            }
        }
        // The arm built the first slice already: the fenced retry re-renders
        // the SAME body, so its render boundary rewinds.
        card.acc.render_from = 0;
        let built = card.acc.build_card_with_info();
        let cursor_stage = state::StagedCursorId {
            id: card
                .acc
                .stage_cursor(None, built.cursor.clone(), built.gap.clone()),
            awaiting_seq: None,
        };
        Some(ProjectedSlice {
            card: built.card,
            cursor_stage,
            watermark_stage: card.acc.pending_watermark_id(),
            full: built.full,
        })
    }
}

/// Release a session's busy guard. A free function so the phases' error paths
/// can release before any [`Turn`] state is settled; idempotent.
pub(super) async fn release_inflight(handles: &TurnHandles, session_id: &str) {
    let mut inflight = handles.waits.inflight.lock().await;
    inflight.remove(session_id);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::test_support::{MockBackend, build_app, realistic_parts, test_config, test_work_dir};

    /// An armed projection's takeover is ONE cards-map critical section (spec
    /// #561, review #569): the verify, the successor attach and the record
    /// re-point share one lock acquisition, so a fresh Turn's card insert —
    /// which takes the same lock — can only land entirely before (Lost,
    /// untouched) or entirely after, never between the check and the attach.
    ///
    /// The race is made deterministic with the map lock itself: while the test
    /// holds it, the takeover and the replacement queue on it in spawn order.
    /// A separate check-then-act implementation releases the lock between its
    /// check and its attach; the queued replacement then slips in first and
    /// the attach lands on the replacement's session — the assertion below
    /// pins that it never does. (The single-threaded test runtime makes the
    /// signal→lock sequence of each task uninterrupted, so the queue order is
    /// the spawn order; the queue itself is FIFO.)
    #[tokio::test]
    async fn the_armed_takeover_is_one_critical_section() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let (app, _platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
        let cards = app.cards_handle();
        Turn::seed_card(&cards, "ses_test", None).await;
        Turn::set_turn_anchor(
            &cards,
            "ses_test",
            &crate::bridge::test_support::turn_anchor(1_000),
        )
        .await;
        let chain_id = {
            let live = cards.cards.lock().await;
            live.get("ses_test").expect("the armed session").chain_id()
        };

        // Hold the map lock so both tasks queue on it, in spawn order.
        let held = cards.cards.lock().await;
        let (ready_tx, mut ready_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        let takeover = {
            let cards = cards.clone();
            let ready = ready_tx.clone();
            tokio::spawn(async move {
                ready.send(()).expect("the test is receiving");
                Turn::take_over_armed_card(
                    &cards,
                    "ses_test",
                    chain_id,
                    "om_late",
                    None,
                    Some("/work"),
                    // Nothing was staged: the id no stage matches.
                    state::StagedCursorId {
                        id: 0,
                        awaiting_seq: None,
                    },
                )
                .await
            })
        };
        let replacement = {
            let cards = cards.clone();
            let ready = ready_tx;
            tokio::spawn(async move {
                ready.send(()).expect("the test is receiving");
                let card = state::CardSession::new(state::StreamAccumulator::new("turn"), None);
                let chain_id = card.chain_id();
                cards.cards.lock().await.insert("ses_test".to_string(), card);
                chain_id
            })
        };
        // Both tasks signalled and parked on the map lock: the takeover first,
        // the fresh Turn's insert second.
        ready_rx.recv().await.expect("the takeover queued");
        ready_rx.recv().await.expect("the replacement queued");
        drop(held);

        assert!(
            matches!(takeover.await.unwrap(), ArmedTakeover::Took(_)),
            "the first-queued takeover wins its own critical section"
        );
        let replacement_chain_id = replacement.await.unwrap();
        assert_ne!(
            replacement_chain_id, chain_id,
            "the replacement really is a different chain"
        );
        let live = cards.cards.lock().await;
        let fresh = live.get("ses_test").expect("the replacement session");
        assert_eq!(
            fresh.chain_id(),
            replacement_chain_id,
            "the replacement session is the one that remains"
        );
        assert!(
            fresh.card_message_id.is_none(),
            "the takeover never attached its card to a session it does not own"
        );
        drop(live);
        assert_eq!(
            cards
                .chains
                .get("ses_test")
                .expect("the takeover tracked its successor")
                .card_message_id,
            "om_late",
            "the record carries the takeover the projection committed before the replacement"
        );
    }

    /// A session that is no longer the armed one is never taken over (spec
    /// #561, review #569): the replacement landing immediately before the
    /// takeover leaves the fresh Turn's session unattached, unseeded and the
    /// record where the Turn put it. The queued-race test above covers the
    /// same verify with lock contention.
    #[tokio::test]
    async fn a_replaced_armed_session_is_never_taken_over() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let (app, _platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
        let cards = app.cards_handle();
        Turn::seed_card(&cards, "ses_test", None).await;
        Turn::set_turn_anchor(
            &cards,
            "ses_test",
            &crate::bridge::test_support::turn_anchor(1_000),
        )
        .await;
        let chain_id = {
            let live = cards.cards.lock().await;
            live.get("ses_test").expect("the armed session").chain_id()
        };
        // The chain the previous life left: the fresh Turn owns it now.
        cards.chains.track(
            "ses_test",
            "om_turn",
            MessageId::new("msg_cola_new"),
            Some(2_000),
            Some("/work"),
            None,
        );

        // A fresh Turn replaced the armed session — its own insert, exactly as
        // `Turn::start` performs it.
        cards.cards.lock().await.insert(
            "ses_test".to_string(),
            state::CardSession::new(state::StreamAccumulator::new("turn"), None),
        );

        assert!(
            matches!(
                Turn::take_over_armed_card(
                    &cards,
                    "ses_test",
                    chain_id,
                    "om_late",
                    None,
                    Some("/work"),
                    state::StagedCursorId {
                        id: 0,
                        awaiting_seq: None,
                    },
                )
                .await,
                ArmedTakeover::Lost
            ),
            "a replaced armed session is lost, never taken over"
        );
        let live = cards.cards.lock().await;
        let fresh = live.get("ses_test").expect("the replacement session");
        assert!(
            fresh.card_message_id.is_none(),
            "a replaced session is never attached"
        );
        assert_eq!(
            *fresh.acc.cursor(),
            RenderedCursor::default(),
            "no carried cursor is seeded into a replaced session"
        );
        drop(live);
        assert_eq!(
            cards
                .chains
                .get("ses_test")
                .expect("the Turn's record survives")
                .card_message_id,
            "om_turn",
            "a replaced session's record is never re-pointed"
        );
    }
}
