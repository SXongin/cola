//! Session Sync's durable Live Card reap (ADR-0063, #438): every tick, each
//! record in [`ChainRecords`](crate::bridge::chain::ChainRecords) is reconciled
//! against the Session's own reads, so a card a cola restart orphaned stops
//! looking live.
//!
//! The pass reaps *state*, never content — with one deliberate exception: a
//! readable transcript's real ending settles the card in place (✅ / ❌ /
//! ⏳ 等待后台任务), an idle Session whose Turn message never landed ends it
//! 「⚠️ 这条消息未被接收」 — never ✅ — and a still-live Session keeps the
//! record: its cursor-carrying chain is adopted and followed (below), while a
//! cursorless one has its orphaned card stamped once with the restart status
//! (#443, the one-release fallback). A
//! card a successor took over is collected as 「⏳ 已由新卡片接管 · 已停止更新」
//! at the takeover itself ([`collect_orphan`], called by the card paths that
//! arm over an orphan), so two cards never both look live. Every ending PATCH
//! keeps the card's already-rendered body best-effort (#434 acceptance
//! feedback): the reap reads the card's own view, strips the controls a
//! whole-card read cannot preserve and restamps the header over the kept
//! elements; a failed read degrades to the bare ending. The exception is the
//! **projection** (spec #561): a record carrying a Rendered Cursor projects
//! the content after the confirmed frontier onto a successor card and
//! collects the recorded card as taken over. A run that ended while cola was
//! down renders its missed tail plus the transcript's true ending and settles
//! (ticket #563, [`project_card`]); a run that is STILL LIVE is adopted and
//! followed — the successor streams what the run produces next and settles by
//! transcript truth (ticket #564, [`project_live_card`], whose follow the
//! caller spawns). A cursorless record, a projection that renders nothing
//! new, and one with no deliverable target all keep the in-place behavior, so
//! an upgrade restart and a no-delta reap read exactly as before.
//! The still-live CURSORLESS orphan's one-time stamp (#443, the fallback a
//! record with no Rendered Cursor keeps for one release — a cursor-carrying
//! record is never stamped, ticket #566) reads the
//! same view the other way around: only the header changes, and a failed read
//! or PATCH claims nothing — a bare stamp would wipe the body the stamp exists
//! to keep — so the next pass retries it; a PATCH Feishu *permanently* refuses
//! as card content is given up for the process life instead (#522). The attempt
//! is handed to a detached
//! task and its write is never cancelled: the pass must not await a stuck
//! Feishu call, while an issued write must run to its own result so the
//! card-delivery lock can order a successor's later collect after it. A
//! per-record in-flight claim holds every other reap decision until the
//! attempt resolves. Content the chain never showed is published by the
//! projections above (bounded by their confirmed cursor), by the
//! existing continuation machinery (a Wake's continuation card), or left in
//! the transcript — the reap never replays a turn onto a stale card, and never
//! re-renders anything at or before a confirmed frontier.
//!
//! The pass's claim — a live Turn (or the follow that inherited its guard)
//! owns the session, or an inbound message is about to — comes from the one
//! **card ownership verdict** ([`CardOwnership::reap_claim`], ADR-0070), never
//! from a second read of the waits state. Everything else the ladder decides on
//! is the record-relative probe ([`decision`]): what the record names, its
//! terminality, the pending ending write, the successor's armed anchor.
//!
//! The pass is a pure decision plus a thin apply (ADR-0069's delivery cuts):
//! [`decision::reconcile`] mirrors every outcome as one [`ChainDisposition`]
//! over the evidence [`RecoveryReads`] carries — no I/O, failures are values —
//! while this module gathers the reads the decision's own plan warrants,
//! performs the card writes, and keeps the card mechanics (the preserved
//! body, the detached stamp attempt). The plan lives beside the decision, so
//! the reads are exactly the ones the decision consumes and a read the pass
//! could not make degrades to "nothing claimed" instead of a wrong claim.
//!
//! A Session that relocated while the run was in flight (#428: `session_move`
//! into a git worktree) gains one line naming the move when its card reaches a
//! terminal ending (#439): the record's directory — the mapping's when the
//! record carries none — is the baseline, the store's session list is the
//! current fact where the generation exposes the canonical one, and a
//! difference between them is the move. That explains the interruption the
//! ending alone would leave mysterious, and it carries no chat content — only
//! the new directory, a server fact. A Waiting yield is not a settle and gets
//! no line; V1 move awareness is #433's, so on a V1 server without the
//! experimental list route the line may simply not render.
//!
//! Every action leaves one INFO line carrying the session and the decision —
//! never chat content; a settle that named a move says so too. A deciding read
//! the reap could not make, or could not interpret, claims nothing: the record
//! stays for the next tick. That covers an unrecognised status kind (unknown
//! is not idle) and a record with no directory to route the reads by (a
//! cwd-routed read could be another instance's run on V1). The move line's own
//! read is cosmetic: a failed or missed one only means the ending carries no
//! line, never that the ending is withheld. Neither PATCH takes a session's
//! card-write lock: no in-memory accumulator owns the card it targets (that is
//! the reap's precondition) and the ending is computed whole, then PATCHed
//! once, so there is no read-send-record sequence to serialize.

use super::decision::{self, CardProbe, ChainDisposition, RecoveryReads, Route, StatusRead, TranscriptRead};
use super::records::ChainRecord;
use crate::backend::{SessionTranscript, TurnAnchor, TurnSettle};
use crate::bridge::chain::RenderedCursor;
use crate::bridge::handles::{CardsHandle, FlowHandles};
use crate::bridge::turn::{ArmedTakeover, CardOwnership, Disposition, Turn};
use crate::feishu::card::{
    CardState, error_line, ledger::TASK_LEDGER_ELEMENT_ID, move_line, shell::CardBuilder,
};

/// Collect the orphaned card `card_message_id` because a new card took the
/// chain over (ADR-0063): one PATCH naming the successor, terminal and grey,
/// keeping whatever the card already showed (#434 acceptance feedback). The
/// seeded takeovers — the fresh-Turn message takeover and the reap's
/// projections — strip the orphan's live tail through
/// [`collect_orphan_after_takeover`]; this ordinary collect (the Wake
/// continuation's arm, the external arm, the reap's successor collect) leaves
/// the preserved body exactly as today. The #443 stamp's repair reproduces
/// the takeover's recorded rule where one exists (this plain collect
/// otherwise).
///
/// A failed PATCH only warns; the record follows the successor either way, so
/// the freeze it leaves behind is the pre-#438 behavior, never a crash.
pub(crate) async fn collect_orphan(cards: &CardsHandle, session_id: &str, card_message_id: &str) {
    collect_orphan_with(cards, session_id, card_message_id, KeepBody::Everything).await;
}

/// Collect the orphaned card `card_message_id` for a seeded takeover — the
/// **fresh-Turn message takeover** and the reap's projections (spec #561,
/// ADR-0068's successor): the Background Task Ledger
/// element goes always (the successor's own reads rebuild the live list,
/// ADR-0060's one-card handover) and each running `⏳` panel goes exactly when
/// its call is in `resolved_calls` — the live-set calls the takeover's seed
/// actually resolved onto the successor (spec #561, review #569). A call the
/// read did not carry keeps its frozen marker: the successor cannot repair
/// what it never saw, so an unresolved panel must not vanish from both cards.
/// A failed, empty or fallback seed resolves nothing and keeps today's body.
/// A failed PATCH only warns, like every collect.
///
/// The rule is recorded on the successor's record **before** the collect's
/// PATCH: a #443 stamp admitted while the takeover ran may land after this
/// collect, and its post-PATCH repair reads the rule to reproduce this strip
/// instead of restoring the tail it removed ([`stamp_restart_attempt`]).
pub(crate) async fn collect_orphan_after_takeover(
    cards: &CardsHandle,
    session_id: &str,
    card_message_id: &str,
    resolved_calls: &[String],
) {
    cards
        .chains
        .note_predecessor_keep(session_id, card_message_id, resolved_calls);
    let keep = KeepBody::WithoutLiveTail {
        resolved_calls: resolved_calls.to_vec(),
    };
    collect_orphan_with(cards, session_id, card_message_id, keep).await;
}

/// Collect a projection's LATE successor — the create that landed after a
/// fresh Turn had already won the chain — **without keeping its body** (spec
/// #561, review #569): the winning Turn's card re-rendered the same tail
/// through the message-first seed, so preserving this body would show the
/// reader the same text twice. The late card is reduced to the bare
/// taken-over marker; every other collect keeps its body (ADR-0063).
pub(crate) async fn collect_late_projection(cards: &CardsHandle, session_id: &str, card_message_id: &str) {
    collect_orphan_with(cards, session_id, card_message_id, KeepBody::Nothing).await;
}

/// The shared takeover collect behind [`collect_orphan`] and
/// [`collect_orphan_after_takeover`]: one PATCH naming the successor, terminal
/// and grey, its preserved body under `keep`.
async fn collect_orphan_with(cards: &CardsHandle, session_id: &str, card_message_id: &str, keep: KeepBody) {
    if card_message_id.is_empty() {
        return;
    }
    let card = ending_card(CardState::TakenOver, None, None);
    match patch_ending_keeping_body(cards.feishu.as_ref(), card_message_id, &card, keep).await {
        // The one reap vocabulary: the INFO line's ending word comes from the
        // state itself, exactly like every `ApplyPass::settle` line.
        Ok(()) => {
            // The collect repainted the card outside the handle registry, so
            // its cached JSON is now older than what Feishu shows (this PATCH
            // stripped every control). Release it: the card's live blocks stay
            // registered — a still-pending request's re-host still moves them
            // to the successor — but the re-host's old-card strip can no
            // longer rewrite this collected presentation from the stale cache
            // (spec #561's adoption × re-host interleaving: a collected card
            // must not be resurrected as a live one).
            cards.card_handles.lock().await.release_cache(card_message_id);
            tracing::info!(
                "live-card reap: session {session_id} {}",
                CardState::TakenOver.reap_word()
            );
        }
        Err(e) => tracing::warn!(
            "live-card reap: session {session_id} could not collect card {card_message_id}: {e}"
        ),
    }
}

/// What one reconcile pass armed and left for its caller to drive (spec #561,
/// ticket #564): a live adoption's follow runs on the External Flow's own
/// cadences and its render loop lives in that module, so the reap returns the
/// fact and the flow spawns the loop. The ended projection spawns nothing —
/// the successor is already terminal — so it never produces one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AdoptedFollow {
    /// The successor card the adoption created.
    pub(crate) card_message_id: String,
    /// The chain's Turn anchor — the successor's scope, and the identity the
    /// follow loop's guards compare.
    pub(crate) anchor: TurnAnchor,
}

/// Reconcile one record against the Session's own reads (ADR-0063). `directory`
/// is the Session's mapped directory, when it still has one — the fallback
/// route when the record itself carries none. `tracked_directory` is the
/// directory the card was tracked under when the caller knows it independently
/// of the current mapping (the pre-follow directory, #433), used for the move
/// verdict (#439) when the record carries no directory of its own. `record` is
/// the snapshot the caller took from the sidecar. `read_timeout_ms` bounds each
/// of the reads, the Session Sync pass's own request bound (injectable in
/// tests), so a hung server degrades to "nothing claimed" instead of freezing
/// the tick.
///
/// The pass: gather the evidence the decision's plan warrants, decide
/// ([`decision::reconcile`]), apply. The card writes are the apply's, and a
/// read that failed or timed out is a value the decision maps to "nothing
/// claimed". Returns the live adoption's follow when the pass armed one (see
/// [`AdoptedFollow`]) — the caller spawns it.
pub(crate) async fn reconcile(
    handles: &FlowHandles,
    session_id: &str,
    directory: Option<&str>,
    tracked_directory: Option<&str>,
    record: &ChainRecord,
    read_timeout_ms: u64,
) -> Option<AdoptedFollow> {
    let mut reads = gather_reads(handles, session_id, record, directory, tracked_directory).await;
    // The successor branch's collect runs before the decision: its awaited
    // orphan PATCH is exactly where a takeover can arm the successor's anchor,
    // and the pre-split pass re-read the anchor after the collect so an anchor
    // landing in that window re-points the record instead of being missed.
    // Only the anchor is re-read — the id and running stay the pre-collect
    // facts the pre-split pass entered the branch with.
    if reads.needs_successor_collect() {
        collect_orphan(&handles.cards, session_id, &record.card_message_id).await;
        let anchor = Turn::armed_turn_anchor(&handles.cards, session_id).await;
        if let CardProbe::Successor { anchor: slot, .. } = &mut reads.card {
            *slot = anchor;
        }
    }
    if reads.needs_status()
        && let Some(route) = reads.route
    {
        reads.status = Some(read_status(handles, session_id, route.directory, read_timeout_ms).await);
    }
    if reads.needs_transcript() {
        reads.transcript = Some(read_transcript(handles, session_id, read_timeout_ms).await);
    }
    let disposition = decision::reconcile(record, &reads);
    apply(handles, session_id, record, &reads, disposition, read_timeout_ms).await
}

/// The evidence no server read is needed for: the claims, the in-process card
/// probe, and the route. The plan ([`RecoveryReads::needs_status`]) decides
/// from here whether the server reads follow.
async fn gather_reads<'a>(
    handles: &FlowHandles,
    session_id: &str,
    record: &'a ChainRecord,
    directory: Option<&'a str>,
    tracked_directory: Option<&'a str>,
) -> RecoveryReads<'a> {
    // The claim — and the current card's message id the record match reads —
    // come from the one ownership verdict (ADR-0070): a live Turn (or the
    // follow that inherited its guard) owns the session, and an inbound
    // message is about to — either way the card is not orphaned.
    let ownership = CardOwnership::read(&handles.cards, &handles.waits, session_id).await;
    let claimed = ownership.reap_claim();
    // The directory the card was TRACKED under: the record's own when it has
    // one, else the caller's pre-follow value. The followed route can never
    // prove a move — it already names the new location (#433).
    let tracked = record
        .directory
        .as_deref()
        .filter(|directory| !directory.is_empty());
    let route = tracked.or(directory).map(|directory| Route {
        directory,
        baseline: tracked.or(tracked_directory).unwrap_or(directory),
    });
    RecoveryReads {
        claimed,
        stamping: record.restart_stamping,
        card: probe_card(handles, session_id, record, ownership.card_message_id()).await,
        route,
        status: None,
        transcript: None,
        cursor: record.cursor.is_some(),
    }
}

/// What THIS process knows about the session's card, relative to the record:
/// whether the record names the process's current card — and if so, whether
/// that card is terminal and whether the delivery outbox still owes it a write
/// — or the successor in the map instead, with its id, liveness and armed Turn
/// anchor. These record-relative distinctions are the ladder's own; whether a
/// live Turn (or the follow that inherited its guard) owns the session, or a
/// message is being routed to it, is the one ownership verdict's claim
/// ([`CardOwnership::reap_claim`], ADR-0070) — read once by [`gather_reads`],
/// never re-derived here. The current card's id is that same read's
/// [`CardOwnership::card_message_id`], so the record match cannot classify a
/// different card than the claim did; the liveness and the successor's armed
/// anchor stay this process's own reads. The ladder never PATCHes or collects a
/// card this process holds, which [`decision`]'s declaration test pins.
async fn probe_card(
    handles: &FlowHandles,
    session_id: &str,
    record: &ChainRecord,
    current_id: Option<&str>,
) -> CardProbe {
    let Some(current_id) = current_id else {
        return CardProbe::None;
    };
    // A live or yielded card is still running; a terminal one is not.
    let running = Turn::is_running(&handles.cards, session_id).await;
    if current_id == record.card_message_id.as_str() {
        return CardProbe::Recorded {
            terminal: !running,
            // Only a terminal card consults the outbox: a live card is kept by
            // its own lifecycle either way.
            update_pending: !running && handles.cards.feishu.has_pending_card_update(current_id),
        };
    }
    CardProbe::Successor {
        card_message_id: current_id.to_string(),
        running,
        anchor: Turn::armed_turn_anchor(&handles.cards, session_id).await,
    }
}

/// The status read — made only when the plan warrants it ([`gather_reads`] +
/// [`RecoveryReads::needs_status`]), bounded by the pass's own request bound,
/// so a hung server degrades to "nothing claimed" instead of freezing the
/// tick. An unrecognised status kind (`Ok(None)`) is unknown, not idle: it
/// claims nothing.
async fn read_status(
    handles: &FlowHandles,
    session_id: &str,
    directory: &str,
    read_timeout_ms: u64,
) -> StatusRead {
    match crate::bridge::bounded_call(
        "live-card reap status",
        read_timeout_ms,
        handles.backend.session_status(session_id, Some(directory)),
    )
    .await
    {
        Some(Ok(Some(status))) => StatusRead::Named(status),
        Some(Ok(None)) => StatusRead::NoEvidence,
        Some(Err(e)) => {
            tracing::warn!("live-card reap: session {session_id} status read failed: {e}");
            StatusRead::NoEvidence
        }
        None => StatusRead::NoEvidence,
    }
}

/// The transcript read — made only when the plan warrants it
/// ([`RecoveryReads::needs_transcript`]), bounded like [`read_status`]. A
/// failed or timed-out read claims nothing.
async fn read_transcript(handles: &FlowHandles, session_id: &str, read_timeout_ms: u64) -> TranscriptRead {
    match crate::bridge::bounded_call(
        "live-card reap transcript",
        read_timeout_ms,
        handles.backend.transcript(session_id),
    )
    .await
    {
        Some(Ok(transcript)) => TranscriptRead::Read(transcript),
        Some(Err(e)) => {
            tracing::warn!("live-card reap: session {session_id} transcript read failed: {e}");
            TranscriptRead::NoEvidence
        }
        None => TranscriptRead::NoEvidence,
    }
}

/// The card writes one disposition owns. `Keep` is the whole silent arm;
/// `NoDecision` logs the one diagnosis the reads did not (a missing route);
/// the collect arms only move the record — the successor's [`collect_orphan`]
/// already ran before the decision (see [`reconcile`]); the stamp hands off to
/// the detached attempt; the settle PATCHes the ending in place; the ended
/// projection arms, creates, tracks and collects its successor
/// ([`project_card`]); the live adoption does the same and returns the follow
/// its caller must spawn ([`project_live_card`], [`AdoptedFollow`]).
async fn apply(
    handles: &FlowHandles,
    session_id: &str,
    record: &ChainRecord,
    reads: &RecoveryReads<'_>,
    disposition: ChainDisposition,
    read_timeout_ms: u64,
) -> Option<AdoptedFollow> {
    match disposition {
        ChainDisposition::Keep => {}
        ChainDisposition::NoDecision => {
            // The one silent no-decision the reads did not log: no directory
            // to route by. A failed status or transcript read logged itself.
            if matches!(reads.card, CardProbe::None) && reads.route.is_none() {
                tracing::debug!(
                    "live-card reap: session {session_id} has no directory to route its reads; keeping the record"
                );
            }
        }
        ChainDisposition::DiscardRecord => {
            // The apply's own re-check: the decision read the card's terminal
            // state and the outbox from a snapshot, and a card that moved on
            // in between must not lose a record it still owes (an ending write
            // that raced this pass).
            super::release_spent(&handles.cards, session_id).await;
        }
        ChainDisposition::CollectThenRepoint { anchor } => {
            // The successor's collect already ran (before the decision, in
            // `reconcile`); only the record moves here. The decision only
            // repoints a successor probe; a mismatch (the impossible case)
            // claims nothing.
            let CardProbe::Successor { card_message_id, .. } = &reads.card else {
                return None;
            };
            handles.cards.chains.track(
                session_id,
                card_message_id.clone(),
                anchor.message_id.clone(),
                Some(anchor.created_ms),
                // The route belongs to the session, not the card: keep it
                // across the re-point.
                record.directory.as_deref(),
            );
        }
        ChainDisposition::CollectThenRelease => {
            // The collect already ran; the successor cannot carry a record —
            // it settled, or it still has no anchor to scope a reap with.
            handles.cards.chains.release(session_id);
        }
        ChainDisposition::StampRestart => {
            stamp_restarted(handles, session_id, record, read_timeout_ms);
        }
        ChainDisposition::Settle(settle) => {
            settle_card(handles, session_id, record, reads.route, settle, read_timeout_ms).await;
        }
        ChainDisposition::Project { settle, seed } => {
            project_card(handles, session_id, record, reads, settle, seed, read_timeout_ms).await;
        }
        ChainDisposition::ProjectLive { seed } => {
            return project_live_card(handles, session_id, record, reads, seed, read_timeout_ms).await;
        }
    }
    None
}

/// Where a projection's successor card is delivered (spec #561): a reply to
/// the original Turn anchor, else to the recorded card, else a top-level send
/// into the chain's Chat. No deliverable target at all means no projection —
/// today's in-place behavior (the ending settle for an ended run, the
/// one-time stamp for a live one).
enum ProjectTarget {
    /// Reply to this Feishu message (the Turn anchor, or the recorded card).
    Reply(String),
    /// Send at the chain's top level into this Chat.
    TopLevel(String),
}

/// The projection's successor delivery target (spec #561), in the spec's
/// fallback order: the original Turn anchor, then the recorded card, then the
/// chain's top-level Chat. `None` when nothing can reach a card — the caller
/// keeps today's behavior.
fn project_target(
    record: &ChainRecord,
    anchor: Option<&TurnAnchor>,
    chat: Option<&str>,
) -> Option<ProjectTarget> {
    if let Some(message_id) = anchor
        .map(|anchor| anchor.message_id.as_str())
        .filter(|message_id| !message_id.is_empty())
    {
        return Some(ProjectTarget::Reply(message_id.to_string()));
    }
    if !record.card_message_id.is_empty() {
        return Some(ProjectTarget::Reply(record.card_message_id.clone()));
    }
    chat.filter(|chat| !chat.is_empty())
        .map(|chat| ProjectTarget::TopLevel(chat.to_string()))
}

/// The outcome of a projection's continuation chain (spec #561, review #569).
enum ProjectedChain {
    /// Every slice landed; the chain's last card id.
    Complete(String),
    /// The chain stopped with content left — a create failed, the card bound
    /// was hit, or a fresh Turn took the session over. The record keeps the
    /// last confirmed slice's cursor and the next life's projection resumes
    /// from it. The last landed card is the record's current card.
    Stopped(String),
}

/// Send the remaining slices of an oversized projection delta (spec #561,
/// review #569): one bounded create per slice through the same splitter the
/// flush uses, in order, each slice's Rendered Cursor confirmed only after its
/// own create landed — so a stop leaves the cursor exactly over the slices that
/// did land (no omission of earlier content, no duplication later). The
/// existing chain bound caps one projection's cards; the rest waits for the
/// next life's recovery. The single-shot rule stays the caller's: it marks the
/// record `projection_attempted` when the chain stops, so this life never
/// re-posts a slice that may have landed.
async fn send_projected_chain(
    handles: &FlowHandles,
    session_id: &str,
    target: &ProjectTarget,
    chain_id: u64,
    directory: &str,
    first_card_id: &str,
    mut full: bool,
) -> ProjectedChain {
    let mut last = first_card_id.to_string();
    let mut slices = 1usize;
    while full {
        if slices >= crate::bridge::turn::MAX_CARD_CHAIN {
            tracing::info!(
                "live-card reap: session {session_id} stopped its successor chain at the card bound with content left"
            );
            return ProjectedChain::Stopped(last);
        }
        let Some(slice) = Turn::next_projected_slice(&handles.cards, session_id, chain_id).await else {
            // The chain is gone (a fresh Turn owns the session): the content
            // left is no longer ours to render.
            return ProjectedChain::Stopped(last);
        };
        // The same write-ahead intent as the first create (spec #561, review
        // #569), scoped to the last TRACKED slice's card: this slice's create
        // may land before its own re-point, so a crash must leave the durable
        // single-shot fact behind. The re-point consumes it; a definite
        // non-delivery clears it below so a later life may resume the chain.
        handles.cards.chains.note_projection_intent(session_id, &last);
        let delivered = match target {
            ProjectTarget::Reply(reply_to) => handles.cards.feishu.reply_card(reply_to, &slice.card).await,
            ProjectTarget::TopLevel(chat) => {
                handles.cards.feishu.send_card("chat_id", chat, &slice.card).await
            }
        };
        let new_card_id = match delivered {
            Ok(card_id) => card_id,
            Err(e) => {
                tracing::warn!(
                    "live-card reap: session {session_id} could not continue its successor chain: {e}"
                );
                if e.is_definite_non_delivery() {
                    // No message was created: the intent is cleared, so a later
                    // life can resume the chain from the last tracked slice.
                    handles.cards.chains.clear_projection_intent(session_id, &last);
                }
                return ProjectedChain::Stopped(last);
            }
        };
        if !Turn::track_projected_continuation(
            &handles.cards,
            session_id,
            chain_id,
            &new_card_id,
            !slice.full,
            Some(directory),
            slice.cursor_stage,
        )
        .await
        {
            // A fresh Turn owns the session: the late card is collected
            // bodyless — the winner re-rendered the same slice — so it cannot
            // look live and the reader never sees the text twice, and the
            // chain stops.
            collect_late_projection(&handles.cards, session_id, &new_card_id).await;
            return ProjectedChain::Stopped(last);
        }
        crate::bridge::turn::drain_armed_watermark(
            &handles.cards,
            session_id,
            chain_id,
            slice.watermark_stage,
        )
        .await;
        last = new_card_id;
        full = slice.full;
        slices += 1;
    }
    ProjectedChain::Complete(last)
}

/// What a projection's shared transition produced (spec #561, review #569).
enum Projection {
    /// The armed successor rendered nothing and the session was still ours:
    /// only the ended projection reaches this, and it takes today's in-place
    /// settle.
    NothingRendered,
    /// The create failed DEFINITELY (the platform created no message): the
    /// record stays retryable and the next reconcile pass re-attempts.
    Retryable,
    /// The chain stopped — an ambiguous failure, a suspended payload, a lost
    /// create window, or the armed session was no longer ours — with NO card
    /// the pass could follow. The record is marked where it is ours and the
    /// caller owes nothing more this life.
    Stopped,
    /// The successor chain stopped after at least one slice LANDED — a
    /// continuation create failed or the chain bound was reached — carrying
    /// the LAST landed card's id (spec #561, review #569). A live adoption
    /// still follows it: the follow's own flush/continuation machinery carries
    /// the remaining delta and the run's future output on that card, because
    /// the single-shot mark blocks further projections, never a follow. The
    /// ended projection treats it as [`Self::Stopped`].
    ChainStopped(String),
    /// Every slice landed; the chain's last card id.
    Landed(String),
}

/// The projection transition both dispositions share (spec #561, review
/// #569): arm the successor from the chain's cursor, send its first create —
/// with the definite-failure classification and the flush's fenced fallback —
/// take the chain over in one cards-map critical section that confirms the
/// create's cursor with the re-point, collect the recorded card and continue
/// an oversized delta as a bounded chain. The callers keep only their genuine
/// differences: `ending` is the ended projection's ending (a live adoption
/// passes `None` and keeps its follow), and `None` means this pass claims
/// nothing.
#[allow(clippy::too_many_arguments)] // the projection's whole fixture
async fn send_projected_successor(
    handles: &FlowHandles,
    session_id: &str,
    record: &ChainRecord,
    target: &ProjectTarget,
    anchor: &TurnAnchor,
    cursor: &RenderedCursor,
    seed: &crate::bridge::turn::CursorSeed,
    transcript: &SessionTranscript,
    ending: Option<&Disposition>,
    route_directory: &str,
    read_timeout_ms: u64,
) -> Option<Projection> {
    let fallback_chat = match target {
        ProjectTarget::Reply(_) => None,
        ProjectTarget::TopLevel(chat) => Some(chat.clone()),
    };
    let title = crate::bridge::external::session_subtitle(
        &handles.backend,
        session_id,
        route_directory,
        read_timeout_ms,
    )
    .await;
    let variant = handles
        .sessions
        .store
        .lock()
        .await
        .entry_for_session(session_id)
        .and_then(|entry| entry.variant.clone());
    let projected = Turn::arm_projected_card(
        &handles.cards,
        session_id,
        anchor,
        cursor,
        seed,
        transcript,
        ending,
        &title,
        route_directory,
        fallback_chat.as_deref(),
        variant,
    )
    .await?;
    let chain_id = projected.chain_id;
    if !projected.rendered && ending.is_some() {
        // The cursor covered the whole read — nothing was missed: drop the
        // armed successor and take today's in-place ending, but only while the
        // session is still the one this pass armed (a fresh Turn that replaced
        // it is never removed and never settled). A live adoption sends either
        // way: the run may produce next.
        return if Turn::drop_armed_session(&handles.cards, session_id, chain_id).await {
            Some(Projection::NothingRendered)
        } else {
            Some(Projection::Stopped)
        };
    }
    // The first slice to send — the armed body, or its fenced rebuild after a
    // definite content rejection.
    let mut slice = crate::bridge::turn::ProjectedSlice {
        card: projected.card,
        cursor_stage: projected.cursor_stage,
        watermark_stage: projected.watermark_stage,
        full: projected.full,
    };
    // The write-ahead intent (spec #561, review #569), persisted BEFORE the
    // create: a process that dies between the create landing and the takeover's
    // re-point leaves this durable single-shot fact on the record, so the next
    // life treats the create as ambiguous and never re-posts a duplicate
    // successor (the old card takes the state-repair path). The success path
    // consumes it — the takeover rewrites the record — and a definite
    // non-delivery clears it below so a retry stays safe.
    handles
        .cards
        .chains
        .note_projection_intent(session_id, &record.card_message_id);
    let new_card_id = loop {
        let delivered = match target {
            ProjectTarget::Reply(reply_to) => handles.cards.feishu.reply_card(reply_to, &slice.card).await,
            ProjectTarget::TopLevel(chat) => {
                handles.cards.feishu.send_card("chat_id", chat, &slice.card).await
            }
        };
        match delivered {
            Ok(card_id) => break card_id,
            Err(e) => {
                tracing::warn!(
                    "live-card reap: session {session_id} could not project its missed tail onto a successor: {e}"
                );
                if matches!(e, crate::error::BridgeError::CardContentRejected { .. }) {
                    // The same payload can never land: re-render THIS slice
                    // through the flush's fenced fallback and retry once. No
                    // duplicate is possible — the rejection proves the first
                    // create made no message. `None` means the fenced retry
                    // was refused too: the payload is suspended, permanently
                    // unlandable.
                    let Some(fenced) =
                        Turn::fenced_projected_slice(&handles.cards, session_id, chain_id).await
                    else {
                        handles
                            .cards
                            .chains
                            .mark_projection_attempted(session_id, &record.card_message_id);
                        // The rejection proves the platform created no message,
                        // so a later life may retry (and re-refuse) rather than
                        // being blocked by a stale intent.
                        handles
                            .cards
                            .chains
                            .clear_projection_intent(session_id, &record.card_message_id);
                        Turn::drop_armed_session(&handles.cards, session_id, chain_id).await;
                        return Some(Projection::Stopped);
                    };
                    slice = fenced;
                    continue;
                }
                if e.is_definite_non_delivery() {
                    // The platform created no message: the record stays
                    // retryable and the next pass re-attempts (spec #561,
                    // review #569) — and the write-ahead intent is cleared, so
                    // a restart may retry too.
                    handles
                        .cards
                        .chains
                        .clear_projection_intent(session_id, &record.card_message_id);
                    Turn::drop_armed_session(&handles.cards, session_id, chain_id).await;
                    return Some(Projection::Retryable);
                }
                // Ambiguous — the send may have landed: single-shot. The mark
                // keeps every later pass from re-posting, the durable intent
                // keeps every later LIFE from re-posting (review #569), and the
                // record stays for the reap's in-place state repair. The phantom
                // session is
                // dropped only while it is still this pass's armed one.
                handles
                    .cards
                    .chains
                    .mark_projection_attempted(session_id, &record.card_message_id);
                Turn::drop_armed_session(&handles.cards, session_id, chain_id).await;
                return Some(Projection::Stopped);
            }
        }
    };
    // The successor takes the chain over in ONE cards-map critical section
    // (review #569): verify the armed session is still current, attach its
    // identity, re-point the record and confirm the create's staged Rendered
    // Cursor — with no gap for a fresh Turn's own card insert to land in. Lost
    // means the Turn owns the session and the record now: the late card is
    // collected so it cannot look live, and none of this pass's takeover
    // writes (the cursor confirm included) may touch the Turn's chain.
    let ArmedTakeover::Took(orphan) = Turn::take_over_armed_card(
        &handles.cards,
        session_id,
        chain_id,
        &new_card_id,
        Some(route_directory),
        slice.cursor_stage,
    )
    .await
    else {
        // The late card is collected WITHOUT its body: the winning Turn's
        // message-first seed already re-rendered the same tail, so preserving
        // it would show the reader the text twice (review #569).
        collect_late_projection(&handles.cards, session_id, &new_card_id).await;
        tracing::info!(
            "live-card reap: session {session_id} lost the create window to a fresh Turn; its late card is collected"
        );
        return Some(Projection::Stopped);
    };
    // The same write carried every Wake completion entry the seeded render
    // staged: the confirmed create is what makes the announcement durable
    // (ADR-0061, ticket #566), so a later recordless restart cannot
    // re-announce the Wake through the Fresh gate. Drained before the old
    // card's collect, so no awaited network call sits between the takeover's
    // cursor confirmation and the record that carries it (review #569).
    crate::bridge::turn::drain_armed_watermark(&handles.cards, session_id, chain_id, slice.watermark_stage)
        .await;
    if let Some(orphan) = orphan {
        crate::bridge::chain::collect_orphan_after_takeover(
            &handles.cards,
            session_id,
            &orphan.card_message_id,
            &seed.resolved_calls(),
        )
        .await;
    }
    // An oversized delta continues on a bounded chain of cards (spec #561,
    // review #569): every slice through the same splitter, each confirmed only
    // after its own create lands. A stopped chain leaves the tail to the next
    // life's recovery: the record (kept by the single-shot mark) holds the
    // last confirmed slice's cursor, and this life re-posts nothing.
    let final_card_id = if slice.full {
        match send_projected_chain(
            handles,
            session_id,
            target,
            chain_id,
            route_directory,
            &new_card_id,
            true,
        )
        .await
        {
            ProjectedChain::Complete(last) => last,
            ProjectedChain::Stopped(last) => {
                handles.cards.chains.mark_projection_attempted(session_id, &last);
                // The chain stopped, but `last` is a card the slices landed on:
                // a live adoption still gets followed there (review #569) —
                // the follow carries the remaining delta and the run's future
                // output, while the single-shot mark keeps later reap passes
                // from re-posting the chain. The ended projection treats it as
                // `Stopped` (nothing more to drive).
                return Some(Projection::ChainStopped(last));
            }
        }
    } else {
        new_card_id
    };
    Some(Projection::Landed(final_card_id))
}

/// Project a run that ended while cola was down (spec #561, ticket #563): arm
/// a successor card seeded from the chain's Rendered Cursor, render the read's
/// missed tail plus the transcript's true ending once, send it as a create,
/// take the chain over and collect the recorded card as taken over. The send
/// is the restart notification (never outbox-retried).
///
/// The guards: a missing route or transcript and an ending with no card state
/// claim nothing; no deliverable target falls back to today's in-place settle;
/// a projection that renders nothing new (the cursor covered the whole read)
/// drops its armed card and settles in place; a DEFINITE create failure leaves
/// the record retryable for the next pass (the missed tail is never given up
/// when the platform provably created nothing), while an ambiguous one is
/// single-shot and leaves the record for the reap's state repair. The shared
/// transition itself lives in [`send_projected_successor`].
async fn project_card(
    handles: &FlowHandles,
    session_id: &str,
    record: &ChainRecord,
    reads: &RecoveryReads<'_>,
    settle: TurnSettle,
    seed: crate::bridge::turn::CursorSeed,
    read_timeout_ms: u64,
) {
    // The decision only projects an orphan with a route and a readable
    // transcript; a missing one here is a plan/decision mismatch.
    let Some(route) = reads.route else {
        return;
    };
    let Some(TranscriptRead::Read(transcript)) = reads.transcript.as_ref() else {
        return;
    };
    let disposition = Disposition::from(settle.clone());
    let Some(state) = disposition.card_state() else {
        // `Observe`: the ending is not decided, so no card is claimed.
        return;
    };
    // The projection's scope: the recorded Turn anchor, else the submitted
    // message's own anchor re-derived from this read (a settle beyond
    // `Unreceived` implies one exists — the decision settled on it). The
    // DELIVERY target follows the same order (spec #561): the original Turn
    // anchor — recorded, or re-derived here when the previous life never
    // persisted its server time — then the recorded card, then the Chat.
    let scope = record
        .anchor()
        .or_else(|| transcript.anchor_of_user(record.message_id.as_str()));
    let chat = handles
        .sessions
        .entry_for_session(session_id)
        .await
        .map(|entry| entry.thread_key.chat_id);
    let Some(target) = project_target(record, scope.as_ref(), chat.as_deref()) else {
        // No deliverable target at all: today's behavior, never a projection.
        settle_card(handles, session_id, record, reads.route, settle, read_timeout_ms).await;
        return;
    };
    let Some(anchor) = scope else {
        // Defensive: a projectable ending carries an anchor, but nothing may
        // be armed without one (the record could not be re-pointed).
        settle_card(handles, session_id, record, reads.route, settle, read_timeout_ms).await;
        return;
    };
    // The durable cursor, re-read at apply time: the record snapshot the reap
    // took before its server reads can be older than a confirmed write that
    // advanced the chain meanwhile. A moved cursor means this pass's decision
    // is stale — claim nothing and let the next pass decide afresh, never
    // re-render content a newer body already delivered.
    let Some(cursor) = handles.cards.chains.cursor(session_id) else {
        return;
    };
    if record.cursor.as_ref() != Some(&cursor) {
        return;
    }
    match send_projected_successor(
        handles,
        session_id,
        record,
        &target,
        &anchor,
        &cursor,
        &seed,
        transcript,
        Some(&disposition),
        route.directory,
        read_timeout_ms,
    )
    .await
    {
        Some(Projection::NothingRendered) => {
            settle_card(handles, session_id, record, reads.route, settle, read_timeout_ms).await;
        }
        Some(Projection::Landed(last)) => {
            if state.is_terminal() {
                // The successor reached a terminal: nothing is owed a reap,
                // and the cursor goes with the record.
                crate::bridge::chain::release_spent(&handles.cards, session_id).await;
            } else if matches!(disposition, Disposition::Waiting) {
                // Waiting: the successor yields, and its own yield is the one
                // the record remembers, so a later pass does not project it
                // again.
                handles.cards.chains.mark_waiting_reaped(session_id, &last);
            }
            tracing::info!(
                "live-card reap: session {session_id} projected its missed tail onto a successor card"
            );
        }
        Some(Projection::Retryable)
        | Some(Projection::Stopped)
        | Some(Projection::ChainStopped(_))
        | None => {}
    }
}

/// Adopt a run that is still live (spec #561, ticket #564): arm a successor
/// card seeded from the chain's Rendered Cursor, render the delta the run has
/// produced so far, send it as a create — the restart notification — take the
/// chain over, collect the recorded card as taken over (dropping the running
/// panels the successor resolved) and hand the caller the successor's identity
/// so it can spawn the follow.
///
/// The guards mirror [`project_card`]'s: a missing route or transcript claims
/// nothing; a missing scope or no deliverable target claims nothing too — a
/// cursor-carrying record is never stamped (spec #561, ticket #566), so an
/// adoption the apply cannot arm waits for a later pass; a cursor the record
/// no longer carries (a confirmed write landed meanwhile) claims nothing and
/// lets the next pass decide afresh. Unlike the ended projection, a successor
/// that renders nothing new is still sent: the run is live and the follow
/// streams whatever it produces next. The shared transition lives in
/// [`send_projected_successor`].
async fn project_live_card(
    handles: &FlowHandles,
    session_id: &str,
    record: &ChainRecord,
    reads: &RecoveryReads<'_>,
    seed: crate::bridge::turn::CursorSeed,
    read_timeout_ms: u64,
) -> Option<AdoptedFollow> {
    // The decision only adopts an orphan with a route and a readable
    // transcript; a missing one here is a plan/decision mismatch.
    let route = reads.route?;
    let Some(TranscriptRead::Read(transcript)) = reads.transcript.as_ref() else {
        return None;
    };
    // The projection's scope: the recorded Turn anchor, else the submitted
    // message's own anchor re-derived from this read. The decision only adopts
    // a record with a scope; a missing one here is a plan/decision mismatch
    // that claims nothing — never the cursorless fallback's stamp (spec #561,
    // ticket #566). The DELIVERY target follows the same order: the original
    // Turn anchor — recorded, or re-derived here — then the recorded card,
    // then the Chat.
    let anchor = record
        .anchor()
        .or_else(|| transcript.anchor_of_user(record.message_id.as_str()))?;
    let chat = handles
        .sessions
        .entry_for_session(session_id)
        .await
        .map(|entry| entry.thread_key.chat_id);
    let Some(target) = project_target(record, Some(&anchor), chat.as_deref()) else {
        // No deliverable target at all: the adoption is not attempted — and a
        // cursor-carrying record is never stamped, so the pass claims nothing.
        return None;
    };
    // The durable cursor, re-read at apply time: the record snapshot the reap
    // took before its server reads can be older than a confirmed write that
    // advanced the chain meanwhile. A moved cursor means this pass's decision
    // is stale — claim nothing and let the next pass decide afresh, never
    // re-render content a newer body already delivered.
    let cursor = handles.cards.chains.cursor(session_id)?;
    if record.cursor.as_ref() != Some(&cursor) {
        return None;
    }
    match send_projected_successor(
        handles,
        session_id,
        record,
        &target,
        &anchor,
        &cursor,
        &seed,
        transcript,
        None,
        route.directory,
        read_timeout_ms,
    )
    .await
    {
        Some(Projection::Landed(last)) => {
            tracing::info!(
                "live-card reap: session {session_id} adopted its still-live run onto a successor card"
            );
            Some(AdoptedFollow {
                card_message_id: last,
                anchor,
            })
        }
        // The chain stopped after landing slices, but the run is still live
        // (spec #561, review #569): the last landed card is handed to the
        // follow, whose own flush/continuation machinery carries the remaining
        // delta and everything the run produces next. The single-shot mark
        // blocks further projections, never a follow.
        Some(Projection::ChainStopped(last)) => {
            tracing::info!(
                "live-card reap: session {session_id} adopted its still-live run onto a successor card whose chain stopped early"
            );
            Some(AdoptedFollow {
                card_message_id: last,
                anchor,
            })
        }
        _ => None,
    }
}

/// PATCH the transcript's ending onto the record's card — keeping the body
/// ([`ApplyPass::settle`]) — or mark a landed yield. The ending is the one
/// disposition table's ([`Disposition::from`]): the state the card wears and
/// the failure line it records come from there, exactly as the live paths
/// render them, so a restart-recovered card ends like a live one.
/// `TurnSettle::Running` never arrives (the decision maps an undecided ending
/// to `Keep`); its disposition is [`Disposition::Observe`], which is no ending
/// and claims nothing.
async fn settle_card(
    handles: &FlowHandles,
    session_id: &str,
    record: &ChainRecord,
    route: Option<Route<'_>>,
    settle: TurnSettle,
    read_timeout_ms: u64,
) {
    // The decision only settles an orphan with a route; a missing one here is
    // a plan/decision mismatch that must claim nothing.
    let Some(route) = route else {
        return;
    };
    let pass = ApplyPass {
        handles,
        session_id,
        record,
        baseline_directory: route.baseline,
        read_timeout_ms,
    };
    let disposition = Disposition::from(settle);
    let Some(state) = disposition.card_state() else {
        // `Observe`: the ending is not decided, so no card is claimed.
        return;
    };
    // The waiting mark follows a landed Waiting PATCH alone (ADR-0059).
    let waiting = matches!(disposition, Disposition::Waiting);
    let detail = disposition.failure().map(error_line);
    if pass.settle(state, detail.as_deref()).await && waiting {
        handles
            .cards
            .chains
            .mark_waiting_reaped(session_id, &record.card_message_id);
    }
}

/// One record's apply state for the transcript settle: the handles, the
/// Session id, the record, the baseline directory the move verdict compares
/// against, and the pass's read bound. Grouped so each ending carries only
/// its state and detail.
struct ApplyPass<'a> {
    handles: &'a FlowHandles,
    session_id: &'a str,
    record: &'a ChainRecord,
    baseline_directory: &'a str,
    read_timeout_ms: u64,
}

impl ApplyPass<'_> {
    /// PATCH the record's card into `state` — keeping the card's existing body
    /// best-effort (#434 acceptance feedback) — and, when the state is terminal,
    /// drop the record: nothing is owed a reap any more. A terminal ending whose
    /// Session's current directory differs from the pass's baseline directory
    /// gains one extra line naming the move (#439), on top of `detail` (the
    /// failure's message when there is one); a Waiting yield is not a settle and
    /// carries none. Returns whether the PATCH landed (a failed one keeps the
    /// record for the next tick). One INFO line per action, naming the session
    /// and the decision — never chat content; a settle that named a move says
    /// so.
    async fn settle(&self, state: CardState, detail: Option<&str>) -> bool {
        let terminal = state.is_terminal();
        let move_note = if terminal { self.move_note().await } else { None };
        let card = ending_card(state.clone(), detail, move_note.as_deref());
        if let Err(e) = patch_ending_keeping_body(
            self.handles.cards.feishu.as_ref(),
            &self.record.card_message_id,
            &card,
            KeepBody::Everything,
        )
        .await
        {
            tracing::warn!(
                "live-card reap: session {} could not settle card {}: {e}",
                self.session_id,
                self.record.card_message_id
            );
            return false;
        }
        let moved = if move_note.is_some() {
            " on a session that moved"
        } else {
            ""
        };
        tracing::info!(
            "live-card reap: session {} {}{moved}",
            self.session_id,
            state.reap_word()
        );
        if terminal {
            // Release only the record this ending belongs to (review #569):
            // between the PATCH and here a fresh Turn can take the session over
            // and re-point the chain; that chain keeps its own record.
            self.handles
                .cards
                .chains
                .release_if_card(self.session_id, &self.record.card_message_id);
        }
        true
    }

    /// The one line a settling card carries when the Session's location changed
    /// since the card was tracked (#428, #439): the move named, so an
    /// interruption the ending alone would leave mysterious is explained. The
    /// current directory comes from the Session reads, never from chat: the
    /// moved Session now lives under its new directory, so the directory-routed
    /// reads that got the reap here may not know it. The list read goes through
    /// the shared session-list cache (`SessionsHandle::cached_session_list`), so
    /// at most one settle per cache TTL touches the wire — but that miss is
    /// awaited before the PATCH, so on a hung server the ending can wait up to
    /// `read_timeout_ms`. That price buys a cosmetic line and is bounded; the
    /// ending itself is never withheld for it. A read that fails, a Session the
    /// list does not carry, an empty directory, or the baseline itself all
    /// claim nothing — no line, exactly the pre-#439 card. A project-scoped
    /// session list may omit the moved Session, and then no line renders
    /// either.
    async fn move_note(&self) -> Option<String> {
        let sessions = match crate::bridge::bounded_call(
            "live-card reap session list",
            self.read_timeout_ms,
            self.handles.sessions.cached_session_list(&self.handles.backend),
        )
        .await
        {
            Some(Ok(sessions)) => sessions,
            Some(Err(e)) => {
                tracing::warn!(
                    "live-card reap: session {} session list read failed: {e}",
                    self.session_id
                );
                return None;
            }
            None => return None,
        };
        let current = sessions
            .iter()
            .find(|session| session.id == self.session_id)?
            .directory
            .as_str();
        if current.is_empty() || current == self.baseline_directory {
            return None;
        }
        Some(move_line(current))
    }
}

/// Claim the one in-flight stamp attempt for this record and hand it to a
/// detached task (#443): the pass must never await the write, and the write
/// must never be cancelled. Both properties matter — a stuck Feishu call
/// cannot freeze Session Sync if nobody awaits it, and the card's delivery
/// lock is what orders a successor's later collect after the stamp, so a
/// cancelled write that still committed at Feishu could land over that
/// collect. The task marks the record when the PATCH lands, repairs a takeover
/// admitted while it was in flight, and releases the claim (landed or failed)
/// so the next pass may retry.
fn stamp_restarted(handles: &FlowHandles, session_id: &str, record: &ChainRecord, read_timeout_ms: u64) {
    let card_message_id = record.card_message_id.clone();
    if !handles
        .cards
        .chains
        .begin_restart_stamp(session_id, &card_message_id)
    {
        return;
    }
    let cards = handles.cards.clone();
    let session_id = session_id.to_string();
    tokio::spawn(async move {
        stamp_restart_attempt(&cards, &session_id, &card_message_id, read_timeout_ms).await;
        cards.chains.finish_restart_stamp(&session_id, &card_message_id);
    });
}
/// One restart-stamp attempt (#443), detached from the Session Sync pass: the
/// still-live orphan's card view is read back (bounded — nothing has been sent
/// yet, so abandoning a timed-out read is safe) and only the header changes —
/// body kept, controls stripped, exactly like a preserved ending. The PATCH is
/// deliberately **not** bounded: cancelling an issued card write releases the
/// card's delivery lock early, and the write could still commit at Feishu
/// after a successor's collect landed, overwriting it. A failed read or PATCH
/// claims nothing — no bare fallback: the stamp's whole value is the body it
/// preserves — and the caller releases the attempt's claim so the next pass
/// retries; a PATCH permanently refused as card content (#522) is marked given
/// up instead. On success the in-memory mark is set, and a takeover admitted
/// while the write was in flight is repaired by the post-PATCH re-collect —
/// under the fresh-Turn takeover's own keep rule when it recorded one
/// (ADR-0068), never restoring the tail that collect removed.
async fn stamp_restart_attempt(
    cards: &CardsHandle,
    session_id: &str,
    card_message_id: &str,
    read_timeout_ms: u64,
) {
    let platform = cards.feishu.as_ref();
    let view = match crate::bridge::bounded_call(
        "live-card reap stamp view",
        read_timeout_ms,
        platform.get_card_view(card_message_id),
    )
    .await
    {
        Some(Ok(view)) => view,
        Some(Err(e)) => {
            tracing::warn!(
                "live-card reap: session {session_id} could not read card {card_message_id} to stamp the restart: {e}"
            );
            return;
        }
        None => return,
    };
    // The pass's ownership check is stale by now: a Turn may have started —
    // or a successor armed — while the view read was in flight. A takeover
    // attaches the successor id BEFORE it collects this card
    // (`take_over_card`'s attach-then-collect order), so any admitted
    // successor is visible here, and its collect is the later terminal: the
    // stamp yields to it rather than overwriting it with an interim status.
    if Turn::card_message_id(cards, session_id).await.is_some() {
        return;
    }
    let card = restamped_keeping_body(&ending_card(CardState::Restarted, None, None), &view);
    if let Err(e) = platform.update_message(card_message_id, &card).await {
        tracing::warn!("live-card reap: session {session_id} could not stamp card {card_message_id}: {e}");
        // #522: a definite content rejection is deterministic — the same
        // preserved payload can never land — so this process life gives the
        // stamp up instead of retrying it every Session Sync tick. Transient
        // failures change nothing: the caller releases the claim either way,
        // and the next pass retries them.
        if matches!(e, crate::error::BridgeError::CardContentRejected { .. }) {
            cards
                .chains
                .mark_restart_stamp_rejected(session_id, card_message_id);
        }
        return;
    }
    tracing::info!(
        "live-card reap: session {session_id} {}",
        CardState::Restarted.reap_word()
    );
    cards.chains.mark_restarted_reaped(session_id, card_message_id);
    // A takeover that started while the PATCH was in flight may have seen its
    // older collect overwritten by this stamp (the delivery lock serializes
    // the two writes, not their order of intent): if a successor owns the
    // session now, collect the orphan again so the takeover has the card's
    // last word. A collect that lands after this PATCH wins on its own; this
    // only repairs the reversed order.
    //
    // The repair reproduces the takeover's own keep rule (ADR-0068), never
    // the plain one: a fresh Turn's collect recorded which of the carry's calls
    // it resolved onto the successor, so the stamp's stale body must not
    // restore the running markers that collect removed. A takeover
    // that recorded no rule — the Wake continuation's arm, the external arm —
    // keeps today's preserved body ([`KeepBody::Everything`]).
    if Turn::card_message_id(cards, session_id).await.is_some() {
        let keep = match cards.chains.predecessor_keep_strip(session_id, card_message_id) {
            Some(resolved_calls) => KeepBody::WithoutLiveTail { resolved_calls },
            None => KeepBody::Everything,
        };
        collect_orphan_with(cards, session_id, card_message_id, keep).await;
    }
}

/// A bare card carrying one ending — the reap's whole card vocabulary when the
/// card's own view cannot be read. The lost turn's content is deliberately NOT
/// rebuilt (ADR-0063): the ending's header, the failure's own message when
/// there is one, the move line when the Session relocated (#439), and no
/// action. On the PATCH path this bare ending is merged with the card's
/// currently rendered view best-effort ([`restamped_keeping_body`], #434
/// acceptance feedback): the card keeps whatever it already showed, while
/// content it never showed — the lost turn's missing work — is still never
/// rebuilt. The Unreceived ending is therefore actionless here — the restart
/// took the accumulator its 重新发起 click would claim (the original prompt and
/// the card session), so the button could only be dead; the user re-sends
/// instead (ADR-0062's amendment).
fn ending_card(state: CardState, detail: Option<&str>, move_note: Option<&str>) -> serde_json::Value {
    let mut builder = CardBuilder::new().with_state(state);
    if let Some(detail) = detail.filter(|detail| !detail.is_empty()) {
        builder = builder.with_text(detail);
    }
    if let Some(note) = move_note {
        builder = builder.with_text(note);
    }
    builder.build()
}

/// PATCH `bare` onto `card_message_id`, keeping the card's existing body
/// best-effort (#434 acceptance feedback): read the card's own view, merge the
/// ending over it, PATCH the merge. `keep` is the merge's view-element rule —
/// [`KeepBody::Everything`] for every ordinary collect, ending and the #443
/// stamp, [`KeepBody::WithoutLiveTail`] for the fresh-Turn takeover's collect
/// and the #443 stamp's repair of it (ADR-0068). A failed
/// read PATCHes `bare` directly — today's behavior — because the ending must
/// never depend on the read.
///
/// A PATCH the platform *definitely* refuses as card content (the typed
/// `CardContentRejected`, e.g. `230099`) is retried once bare: the refusal is
/// deterministic, so the bare ending lands and the card never stays looking
/// live. Any other failure — transport, timeout, auth, server — is returned
/// as-is: the preserved PATCH may already have landed, and retrying bare would
/// then wipe the very body this path exists to keep.
async fn patch_ending_keeping_body(
    platform: &dyn crate::feishu::Platform,
    card_message_id: &str,
    bare: &serde_json::Value,
    keep: KeepBody,
) -> crate::error::Result<()> {
    match platform.get_card_view(card_message_id).await {
        Ok(view) => {
            let card = restamped_keeping_body_with(bare, &view, keep);
            match platform.update_message(card_message_id, &card).await {
                Ok(()) => Ok(()),
                Err(e @ crate::error::BridgeError::CardContentRejected { .. }) => {
                    tracing::warn!(
                        "live-card reap: card {card_message_id} refused the preserved ending ({e}); retrying bare"
                    );
                    platform.update_message(card_message_id, bare).await
                }
                Err(e) => Err(e),
            }
        }
        Err(e) => {
            tracing::debug!("live-card reap: card {card_message_id} view unreadable ({e}); settling bare");
            platform.update_message(card_message_id, bare).await
        }
    }
}

/// The view-element rule a preserved card's merge applies (#434 acceptance
/// feedback, ADR-0068).
#[derive(Clone, Debug, PartialEq, Eq)]
enum KeepBody {
    /// Every ordinary collect, every ending and the #443 restart stamp —
    /// plus the stamp's repair when no takeover recorded a rule: the card's
    /// own view stays as it was, its controls stripped.
    Everything,
    /// The fresh-Turn message takeover's collect and the #443 stamp's repair
    /// of it (ADR-0068, the one collect the spec's Trigger scopes the strip
    /// to): the view's live tail goes —
    /// the Background Task Ledger element always, because the successor's own
    /// reads rebuild the live list (ADR-0060), and each running `⏳` panel
    /// whose call is in `resolved_calls` (spec #561, review #569) — the
    /// panels the successor actually carries. Every other element stays,
    /// including a running marker whose call the read did not carry: the
    /// successor cannot repair what it never saw.
    WithoutLiveTail { resolved_calls: Vec<String> },
    /// The late projection successor's collect (spec #561, review #569): the
    /// card is reduced to the bare taken-over marker, NO element kept. The
    /// winner (a fresh Turn whose message-first seed re-rendered the same
    /// tail) already shows that content, so preserving this body would make
    /// the reader read the same text twice. The one deliberate departure from
    /// ADR-0063's body-preserving collect.
    Nothing,
}

impl KeepBody {
    /// Whether the preserved body keeps `element` (before the interactive
    /// strip).
    fn keeps(&self, element: &serde_json::Value) -> bool {
        match self {
            Self::Everything => true,
            Self::WithoutLiveTail { resolved_calls } => {
                if is_task_ledger_element(element) {
                    return false;
                }
                !is_running_panel_element(element) || !names_resolved_call(element, resolved_calls)
            }
            Self::Nothing => false,
        }
    }
}

/// Whether the running panel `element` names a call the seed resolved (spec
/// #561, review #569): a tool panel's element id IS its call
/// (`tool_{call_id}`), so exactly the markers the successor carries leave the
/// collected card. A panel with no id — or one rendered before the id named
/// its call — is kept: an unidentifiable marker is never dropped.
fn names_resolved_call(element: &serde_json::Value, resolved_calls: &[String]) -> bool {
    element
        .get("element_id")
        .and_then(|id| id.as_str())
        .and_then(|id| id.strip_prefix("tool_"))
        .is_some_and(|call_id| resolved_calls.iter().any(|resolved| resolved == call_id))
}

/// Whether `element` is the Background Task Ledger's panel (ADR-0060): the
/// stable `element_id` names it, whatever its title currently counts. The
/// successor's own reads rebuild the live list, so a collect never keeps it.
fn is_task_ledger_element(element: &serde_json::Value) -> bool {
    element.get("element_id").and_then(|id| id.as_str()) == Some(TASK_LEDGER_ELEMENT_ID)
}

/// Whether `element` is a running tool panel as a whole-card read returns it
/// (ADR-0068): a `collapsible_panel` whose plain-text title begins `⏳`. The
/// ledger shares the glyph and is matched by its element id first.
fn is_running_panel_element(element: &serde_json::Value) -> bool {
    element["tag"] == "collapsible_panel"
        && element["header"]["title"]["tag"] == "plain_text"
        && element["header"]["title"]["content"]
            .as_str()
            .is_some_and(|title| title.starts_with('⏳'))
}

/// The bare card — an ending, or the still-live orphan's restart stamp (#443)
/// — restamped over the card's existing view (#434 acceptance feedback): the
/// bare card's header leads, its own elements (the failure's message and/or
/// the move line; none for the stamp) come before the view's body, and every
/// interactive element is stripped from the view's elements — a whole-card read
/// does not return a control's `value`, so a preserved control could only
/// render dead. The view's `config` rules the restamped card (`streaming_mode`
/// forced off so a preserved card never keeps a live-streaming presentation);
/// a view without one keeps the bare card's. Schema 2.0, the one the PATCH API
/// accepts back.
fn restamped_keeping_body(bare: &serde_json::Value, view: &serde_json::Value) -> serde_json::Value {
    restamped_keeping_body_with(bare, view, KeepBody::Everything)
}

/// [`restamped_keeping_body`] with an explicit view-element rule: `keep`
/// decides which of the view's elements the preserved merge carries before the
/// interactive strip.
fn restamped_keeping_body_with(
    bare: &serde_json::Value,
    view: &serde_json::Value,
    keep: KeepBody,
) -> serde_json::Value {
    let mut elements: Vec<serde_json::Value> =
        bare["body"]["elements"].as_array().cloned().unwrap_or_default();
    if let Some(view_elements) = view["body"]["elements"].as_array() {
        elements.extend(
            view_elements
                .iter()
                .filter(|element| keep.keeps(element))
                .filter_map(stripped_of_controls),
        );
    }
    let mut config = view
        .get("config")
        .filter(|config| config.is_object())
        .cloned()
        .or_else(|| bare.get("config").filter(|config| config.is_object()).cloned())
        .unwrap_or_else(|| serde_json::json!({ "wide_screen_mode": true }));
    config["streaming_mode"] = serde_json::json!(false);
    serde_json::json!({
        "schema": "2.0",
        "config": config,
        "header": bare["header"].clone(),
        "body": { "elements": elements },
    })
}

/// The card components that are interactive, or exist only to group controls.
/// A preserved card must never show a control whose action can no longer run
/// (its callback `value` did not survive the whole-card read), so these are
/// stripped wherever they nest: panels, column sets and forms included. The
/// list is Feishu's interactive card-JSON-2.0 components (`checker` is the
/// documented 勾选器) plus the `form` container — `form` goes whole because an
/// emptied form is invalid and every child it can hold is stripped anyway.
const INTERACTIVE_TAGS: &[&str] = &[
    "button",
    "action",
    "input",
    "select_static",
    "multi_select_static",
    "select_person",
    "multi_select_person",
    "overflow",
    "date_picker",
    "picker_time",
    "picker_datetime",
    "select_img",
    "checker",
    "form",
];

/// Whether `tag` names an [`INTERACTIVE_TAGS`] component.
fn is_interactive_element(tag: &str) -> bool {
    INTERACTIVE_TAGS.contains(&tag)
}

/// [`is_interactive_element`]'s recursive application: `None` for a stripped
/// element, the element — its nested arrays and object fields filtered —
/// otherwise. An interactive child is dropped **whole**: leaving a `null`
/// placeholder would itself be invalid card JSON and get the preserved PATCH
/// rejected.
fn stripped_of_controls(element: &serde_json::Value) -> Option<serde_json::Value> {
    if element
        .get("tag")
        .and_then(|tag| tag.as_str())
        .is_some_and(is_interactive_element)
    {
        return None;
    }
    match element {
        serde_json::Value::Object(map) => {
            let mut cleaned = serde_json::Map::with_capacity(map.len());
            for (key, value) in map {
                if let Some(cleaned_value) = stripped_of_controls(value) {
                    cleaned.insert(key.clone(), cleaned_value);
                }
            }
            Some(serde_json::Value::Object(cleaned))
        }
        serde_json::Value::Array(items) => Some(serde_json::Value::Array(
            items.iter().filter_map(stripped_of_controls).collect(),
        )),
        other => Some(other.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reap's card vocabulary is one state + one optional line (plus the
    /// move line when the Session relocated): the ending's header, the failure
    /// message when given, and no recovery button (the restart took the
    /// accumulator that could claim one).
    #[test]
    fn ending_cards_carry_the_state_and_no_action() {
        let done = ending_card(CardState::Done, None, None);
        assert_eq!(done["header"]["title"]["content"], "✅ 完成");
        assert!(done["body"]["elements"].as_array().unwrap().is_empty());

        let failed = ending_card(CardState::Error, Some("**错误**: 503"), None);
        assert_eq!(failed["header"]["title"]["content"], "❌ 出错");
        assert!(
            failed.to_string().contains("503"),
            "the failure's own message rides the card: {failed}"
        );

        let unreceived = ending_card(CardState::Unreceived, None, None);
        assert_eq!(unreceived["header"]["title"]["content"], "⚠️ 这条消息未被接收");
        assert!(
            unreceived["body"]["elements"]
                .as_array()
                .unwrap()
                .iter()
                .all(|element| element["tag"] != "button"),
            "the reap offers no recovery action: {unreceived}"
        );

        let waiting = ending_card(CardState::Waiting, None, None);
        assert_eq!(waiting["header"]["title"]["content"], "⏳ 等待后台任务");
    }

    /// The move line (#439) rides the ending next to its other line: the header
    /// keeps the state, the failure's message and the new directory both
    /// render, and the line is built from the directory alone — no chat
    /// content.
    #[test]
    fn ending_cards_carry_the_move_line_next_to_the_detail() {
        let failed_and_moved = ending_card(
            CardState::Error,
            Some("**错误**: Step interrupted"),
            Some(&move_line("/work/.worktrees/zh-user-guide")),
        );
        assert_eq!(failed_and_moved["header"]["title"]["content"], "❌ 出错");
        let rendered = failed_and_moved.to_string();
        assert!(
            rendered.contains("Step interrupted"),
            "the failure's own message stays: {failed_and_moved}"
        );
        assert!(
            rendered.contains("**会话已迁移**: `/work/.worktrees/zh-user-guide`"),
            "the move line names the new directory: {failed_and_moved}"
        );
        assert_eq!(
            failed_and_moved["body"]["elements"].as_array().unwrap().len(),
            2,
            "the card carries the detail and the move line and nothing else: {failed_and_moved}"
        );

        let unreceived_moved = ending_card(CardState::Unreceived, None, Some(&move_line("/w2")));
        assert_eq!(
            unreceived_moved["body"]["elements"][0]["content"],
            "**会话已迁移**: `/w2`"
        );
    }

    /// The preserved ending (#434 acceptance feedback): the ending's header
    /// replaces the old one, the ending's own line is prepended to the fetched
    /// body, every interactive element is stripped recursively (a nested
    /// button and a whole action block included), and `streaming_mode` is
    /// forced off while the view's other config survives. A view without a
    /// config keeps the ending's.
    #[test]
    fn restamped_keeping_body_prepends_the_line_and_strips_controls() {
        use crate::bridge::test_support::{card_buttons, card_has_tag, card_text};

        let bare = ending_card(CardState::Error, Some("**错误**: 503"), None);
        let view = serde_json::json!({
            "schema": "2.0",
            "config": {
                "wide_screen_mode": true,
                "streaming_mode": true,
                "enable_forward_interaction": false
            },
            "header": {
                "template": "blue",
                "title": { "tag": "plain_text", "content": "✍️ 回复中" }
            },
            "body": { "elements": [
                { "tag": "markdown", "content": "**正文** 第一段" },
                { "tag": "collapsible_panel", "expanded": false, "elements": [
                    { "tag": "markdown", "content": "面板里的输出" },
                    { "tag": "button", "text": { "tag": "plain_text", "content": "重试" },
                      "value": { "action": "retry" } }
                ] },
                { "tag": "action", "actions": [
                    { "tag": "button", "text": { "tag": "plain_text", "content": "重新发起" },
                      "value": { "action": "resume" } }
                ] },
                { "tag": "hr" }
            ] }
        });

        let card = restamped_keeping_body(&bare, &view);
        assert_eq!(card["schema"], "2.0");
        assert_eq!(
            card["header"]["title"]["content"], "❌ 出错",
            "the ending's header replaces the card's old one: {card}"
        );
        assert_eq!(
            card["config"]["streaming_mode"], false,
            "streaming is forced off: {card}"
        );
        assert_eq!(
            card["config"]["wide_screen_mode"], true,
            "other config fields survive: {card}"
        );
        assert_eq!(card["config"]["enable_forward_interaction"], false);
        let elements = card["body"]["elements"].as_array().unwrap();
        assert_eq!(
            elements[0]["content"], "**错误**: 503",
            "the ending's own line is prepended: {card}"
        );
        assert_eq!(
            elements[1]["content"], "**正文** 第一段",
            "the preserved body follows the line: {card}"
        );
        assert!(
            card_text(&card).contains("面板里的输出"),
            "nested panel content survives: {card}"
        );
        assert!(
            card_buttons(&card).is_empty(),
            "no preserved button survives, nested or not: {card}"
        );
        assert!(!card_has_tag(&card, "action"), "no action block survives: {card}");
        assert_eq!(
            elements[2]["tag"], "collapsible_panel",
            "the panel itself survives: {card}"
        );
        assert_eq!(
            elements[2]["elements"].as_array().unwrap().len(),
            1,
            "the panel's nested control is stripped: {card}"
        );
        assert_eq!(
            elements[3]["tag"], "hr",
            "non-interactive elements keep their order: {card}"
        );

        // A view without a config keeps the ending's own (streaming still off).
        let no_config = serde_json::json!({
            "body": { "elements": [{ "tag": "markdown", "content": "旧的正文" }] }
        });
        let card = restamped_keeping_body(&bare, &no_config);
        assert_eq!(card["config"]["wide_screen_mode"], true);
        assert_eq!(card["config"]["streaming_mode"], false);
        assert_eq!(card["body"]["elements"][1]["content"], "旧的正文");
    }

    /// Nested and container controls: an interactive object at a non-array key
    /// is dropped **whole** (a `null` placeholder would itself be invalid card
    /// JSON), a whole `form` goes (an emptied form is invalid and every child
    /// it can hold is stripped anyway), and a `checker`/`select_img` nested in
    /// a column go too — while the display content around them survives.
    #[test]
    fn restamped_keeping_body_drops_containers_without_nulls() {
        use crate::bridge::test_support::{card_has_tag, card_text};

        fn contains_null(value: &serde_json::Value) -> bool {
            match value {
                serde_json::Value::Null => true,
                serde_json::Value::Object(map) => map.values().any(contains_null),
                serde_json::Value::Array(items) => items.iter().any(contains_null),
                _ => false,
            }
        }

        let bare = ending_card(CardState::Done, None, None);
        let view = serde_json::json!({
            "config": { "wide_screen_mode": true },
            "body": { "elements": [
                { "tag": "form", "name": "switch_search", "elements": [
                    { "tag": "input", "name": "search" },
                    { "tag": "button", "text": { "tag": "plain_text", "content": "搜索" },
                      "value": { "action": "submit" } }
                ] },
                { "tag": "collapsible_panel", "elements": [
                    { "tag": "markdown", "content": "面板里的输出" }
                ],
                  // An object-valued key holding a control: the key must go
                  // whole, not become `"button_area": null`.
                  "button_area": { "tag": "button", "value": { "action": "retry" } } },
                { "tag": "column_set", "columns": [
                    { "tag": "column", "elements": [
                        { "tag": "checker", "name": "check_1" },
                        { "tag": "select_img", "name": "pick_1" },
                        { "tag": "markdown", "content": "保留的正文" }
                    ] }
                ] }
            ] }
        });

        let card = restamped_keeping_body(&bare, &view);
        for tag in ["form", "input", "button", "checker", "select_img"] {
            assert!(!card_has_tag(&card, tag), "{tag} must be stripped whole: {card}");
        }
        assert!(
            !contains_null(&card),
            "no null placeholder may survive a stripped child: {card}"
        );
        assert!(
            card_text(&card).contains("面板里的输出") && card_text(&card).contains("保留的正文"),
            "the display content around the controls survives: {card}"
        );
        let panel = &card["body"]["elements"][0];
        assert_eq!(panel["tag"], "collapsible_panel", "{card}");
        assert!(
            panel.get("button_area").is_none(),
            "the object-valued control's key is dropped, never nulled: {card}"
        );
    }

    /// A bare ending with no line of its own prepends nothing: the preserved
    /// body leads the card.
    #[test]
    fn restamped_keeping_body_without_a_line_leads_with_the_view() {
        let bare = ending_card(CardState::Done, None, None);
        let view = serde_json::json!({
            "config": { "wide_screen_mode": true },
            "body": { "elements": [{ "tag": "markdown", "content": "保留的正文" }] }
        });
        let card = restamped_keeping_body(&bare, &view);
        let elements = card["body"]["elements"].as_array().unwrap();
        assert_eq!(elements.len(), 1);
        assert_eq!(elements[0]["content"], "保留的正文");
        assert_eq!(card["header"]["title"]["content"], "✅ 完成");
    }

    /// ADR-0068: the takeover collect's preserved body drops the Background
    /// Task Ledger element always — the successor's own reads rebuild the live
    /// list — and exactly the running `⏳` panels whose call the takeover's
    /// seed resolved onto the successor (`resolved_calls`, matched through the
    /// panel's call-named element id; spec #561, review #569). The #444 probe
    /// ("the collected card keeps a stale running marker today") is inverted
    /// here: with the seed's answer the marker goes, without it today's
    /// preserved body stays; the reap's endings and the #443 stamp
    /// ([`KeepBody::Everything`]) keep both.
    #[test]
    fn the_collect_drops_the_ledger_and_only_a_seeded_running_panel() {
        use crate::bridge::test_support::card_text;

        let bare = ending_card(CardState::TakenOver, None, None);
        // A rendered live card's tail as a whole-card read returns it: the
        // written body, a running tool panel, a settled backgrounded launch
        // and the ledger.
        let view = serde_json::json!({
            "config": { "wide_screen_mode": true },
            "body": { "elements": [
                { "tag": "markdown", "content": "**正文** 已经写完的部分" },
                { "tag": "collapsible_panel", "expanded": false, "element_id": "tool_call_1",
                  "header": { "title": { "tag": "plain_text", "content": "⏳ shell · 12:00" } },
                  "elements": [ { "tag": "markdown", "content": "**Input**\n`sleep 600`" } ] },
                { "tag": "collapsible_panel", "expanded": false, "element_id": "tool_2",
                  "header": { "title": { "tag": "plain_text", "content": "🌙 shell · 12:01" } },
                  "elements": [ { "tag": "markdown", "content": "moved to background" } ] },
                { "tag": "collapsible_panel", "expanded": false, "element_id": "task_ledger",
                  "header": { "title": { "tag": "plain_text", "content": "⏳ 后台任务（1）" } },
                  "elements": [ { "tag": "markdown",
                                  "content": "· shell：**npm run build** · 12:00 · 3m12s" } ] }
            ] }
        });

        // The carry moved the running panel: it and the ledger leave the
        // collected body; the written content and the settled launch stay.
        let carried = restamped_keeping_body_with(
            &bare,
            &view,
            KeepBody::WithoutLiveTail {
                resolved_calls: vec!["call_1".to_string()],
            },
        );
        let text = card_text(&carried);
        assert!(
            !text.contains("⏳ shell"),
            "a carried running marker leaves the collected card: {text}"
        );
        assert!(
            !text.contains("⏳ 后台任务"),
            "the ledger leaves every takeover collect: {text}"
        );
        assert!(
            text.contains("🌙 shell") && text.contains("moved to background"),
            "the settled backgrounded launch stays: {text}"
        );
        assert!(
            text.contains("**正文** 已经写完的部分"),
            "non-tail body content is preserved: {text}"
        );
        assert!(
            !carried["body"]["elements"]
                .as_array()
                .unwrap()
                .iter()
                .any(|element| element["element_id"] == "task_ledger"),
            "the ledger element itself is gone: {carried}"
        );

        // Nothing carried: the running marker stays (today's preserved body);
        // the ledger still goes.
        let kept = restamped_keeping_body_with(
            &bare,
            &view,
            KeepBody::WithoutLiveTail {
                resolved_calls: Vec::new(),
            },
        );
        let text = card_text(&kept);
        assert!(
            text.contains("⏳ shell"),
            "an uncarried running marker stays: {text}"
        );
        assert!(
            !text.contains("⏳ 后台任务"),
            "the ledger leaves either way: {text}"
        );

        // Every other preserved card — the reap's endings and the #443 stamp —
        // keeps the view as it was.
        let ending = restamped_keeping_body(&bare, &view);
        let text = card_text(&ending);
        assert!(
            text.contains("⏳ shell") && text.contains("⏳ 后台任务"),
            "the endings keep the marker and the ledger: {text}"
        );
    }
}
