mod disposition;
mod flush;
mod follow;
mod interaction;
mod ownership;
mod render;
mod settle;
mod state;
mod wake;

/// The Wake continuation's decision vocabulary and the projection's successor
/// types (ADR-0059, ADR-0071): defined in [`wake`] and re-exported here so
/// every external `turn::…` path — `bridge/external.rs`, `chain/reconcile.rs`,
/// `turn/state.rs`'s card session — is unchanged by the module split.
pub(crate) use wake::{ArmedTakeover, ContinuationFacts, ContinuationLine, ProjectedSlice, WakeContinuation};

/// `ProjectedCard` (spec #561) carried the `turn::ProjectedCard` path before the
/// split. #617 requires external call paths to stay unchanged, so the path is
/// preserved even though no caller names it today — `arm_projected_card`'s
/// result is consumed field-wise in `chain/reconcile.rs`.
#[allow(unused_imports)]
pub(crate) use wake::ProjectedCard;

/// The busy-guard release the phases call before any settled state (a free
/// function in [`wake`]): re-exported so its callers keep their paths.
use wake::release_inflight;

/// The one card-session type the coordinator's map holds. Its accumulator and
/// identity chain stay private to the Turn module: every read/write goes
/// through the interface above (spec #298, A3).
pub(crate) use state::CardSession;

/// The Rendered Cursor's drain reconcile (spec #561): after the Platform's
/// Pending Card Update drain, Session Sync advances every staged cursor whose
/// owed payload has delivered — while a projection's successor create confirms
/// its staged cursor inside its own atomic takeover, before any later await
/// (ticket #563, review #569).
pub(crate) use flush::{drain_armed_watermark, reconcile_staged_cursors};

/// What became of a flush's write(s) (review, PR #595): the disposition the
/// render/refresh/split carriers report so a reconcile pass commits only on an
/// accepted write.
pub(crate) use flush::FlushOutcome;

/// The drain reconcile's test seam (spec #561, review #569): the gate parks
/// the pass between its delivery check and the confirmation.
#[cfg(test)]
pub(crate) use flush::{ReconcileGate, reconcile_staged_cursors_gated};

/// The confirmation's test seams (spec #561, review #569): the gate parks one
/// confirmation between its stage take and its durable write, and the wrapper
/// confirms one exact stage through the same path the flush uses.
#[cfg(test)]
pub(crate) use flush::{ConfirmGate, confirm_staged_cursor_for_test};

/// The projection's resolved render seed (spec #561, ticket #563): the chain's
/// Rendered Cursor placed in one transcript read, which the Chain Record
/// module's decision carries and this module's render consumes.
pub(crate) use state::CursorSeed;

/// The one card ownership verdict (ADR-0070, spec #545): computed by one read
/// over the waits state and the card map; the prompt router, the Wake gate,
/// the reap and the `/stop` acknowledgement read its named rules. Its module
/// docs state the sources.
pub(crate) use ownership::{CardClass, CardOwnership, StopDisposition};

/// The one ending vocabulary (spec #538): the table every path that ends a
/// card reads — the in-Turn paths through `StreamAccumulator::apply_ending`,
/// the out-of-turn loops through `Ticket::apply_ending_if_owned` (the
/// ownership check and the stamp under one lock), and the durable reap through
/// its `From<TurnSettle>` mapping. Crate-visible so a path outside the Turn
/// module (the reap's card translation in `chain::reconcile`) reads the same
/// table instead of a second state/failure mapping.
pub(crate) use disposition::Disposition;

use std::collections::HashMap;
use std::sync::Arc;

use tracing::Instrument;

use crate::backend::{MessageId, MessageRole, SessionTranscript, TurnAnchor, TurnSettle};
use crate::bridge::chain::{ChainRecord, RenderedCursor};
use crate::bridge::handler::image_inputs;
use crate::bridge::handles::{CardsHandle, NoticeRules, RequestsHandle, SessionsHandle, TurnHandles};
use crate::bridge::span;
use crate::bridge::turn::state::{EndingWrite, StreamAccumulator};
use crate::config::ThreadKey;
use crate::feishu::client::ImageAttachment;
use crate::opencode::types::SessionStatus;

/// How many consecutive drain reads must see no run before an unobserved
/// submit is treated as never having registered. A prompt admit only
/// *schedules* execution (ADR-0056), so a single non-busy read can precede the
/// run's registration; the value is the retired V2 poll fallback's confirmation
/// window, kept so the unstarted case is bounded by this window instead of
/// waiting forever. After it the turn finalizes from what it has — a reply
/// arriving later is deliberately not followed (the retired fallback's accepted
/// degradation). A failing or timing-out STATUS read takes the same window; a
/// failing TRANSCRIPT read is the drain's own rule (the merged path's
/// lost-contact grace bounds it).
const IDLE_CONFIRMATIONS: usize = 3;

/// A p2p Turn that ran at least this long notifies on completion (ADR-0043
/// amendment 2026-09-21): a long task's end is the one event worth a new
/// message even when the user was around, because the card patch itself
/// neither pushes a notification nor bumps the conversation. Five minutes is
/// the "long" line; it is a constant, not a config key, and tests inject a
/// tiny value through the turn config's `long_task_notice_ms` atomic.
pub(crate) const LONG_TASK_NOTICE_MS: u64 = 300_000;

/// What one drain read saw (ADR-0043).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DrainState {
    /// Nothing pending: the drain can end.
    Settled,
    /// The session's run is still going (busy, or scheduled for a retry).
    Running,
    /// A cola-authored Supplement newer than the turn anchor has no assistant
    /// reply after it.
    Supplement,
    /// The session is idle but Background Tasks are still live (ADR-0059): the
    /// Turn yields 「⏳ 等待后台任务」 — not Done, no Completion Notice — and
    /// the next Wake continues the chain on a new card.
    Waiting,
    /// The submitted message never reached the transcript and the Session is
    /// idle (ADR-0062): nobody will answer it, so the card ends Unreceived —
    /// 「⚠️ 这条消息未被接收」, never ✅.
    Unreceived,
    /// No full read pair answered for the follow grace: the Backend is wedged
    /// (#284/#386). The merged unbounded drain ends the card Error here rather
    /// than observing forever.
    LostContact,
    /// A readable, settled card still carries a live `⏳` panel past the grace
    /// (a crash-orphaned call, #386): the card ends Error rather than a false
    /// ✅ over an unfinished panel.
    StuckPanel,
    /// The accumulator vanished from under the drain (a replacement or a
    /// collect took the card): nothing is owned any more, so the drain ends
    /// silently — no ending is stamped and no notice is sent.
    Vanished,
}

/// What a non-busy status read said, as [`Turn::settle_or_yield`] reads it
/// (ADR-0062): the Session is not live, or the read could not say. A named
/// fact, so the Unreceived decision cannot confuse "idle" with "unreadable" —
/// an unreadable read is no evidence either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IdleRead {
    /// A readable, non-busy status: the Session is not live.
    Idle,
    /// The status call failed or timed out: nothing may be decided from it.
    Unreadable,
}

/// What one [`Turn::drain_state`] pass classified, plus the read-contact facts
/// the merged unbounded loop's graces watch (#603): whether the status read
/// answered live, answered non-live (the readable, settled read the stuck-panel
/// grace watches), or did not answer at all.
struct DrainRead {
    state: DrainState,
    /// The status read answered and was live (Busy/Retry).
    live: bool,
    /// The transcript and a non-live status both answered: a readable, settled
    /// read, the one the stuck-panel grace watches.
    settled_readable: bool,
    /// Both the transcript and the status read answered.
    contact: bool,
}

/// One drain tick's outcome: the rule state plus the read-contact facts. The
/// state is `None` when the transcript read failed (no rule could be applied).
struct DrainTick {
    state: Option<DrainState>,
    live: bool,
    settled_readable: bool,
    contact: bool,
}

/// What a settled drain's one fresh finalization read found (#604).
enum SettledFinalRead {
    /// The read showed nothing the card lacks: the rendered, quiescent snapshot
    /// the ending is decided on.
    Quiescent(Box<SessionTranscript>),
    /// The read failed: fall back to the drain's last rendered snapshot.
    Unreadable,
    /// The read still carried content the card lacks — the run went live again.
    /// The caller re-opens the drain instead of stamping an ending here.
    Live,
}

/// Everything [`Turn::run`] needs for one turn. Built by `handle_prompt` for a
/// fresh message and by the error-card "retry" action (which re-submits the
/// original prompt under either the failed attempt's id or a fresh one, and
/// replies a NEW card — spec #391).
pub(crate) struct PromptContext {
    pub(crate) session_id: String,
    pub(crate) thread_key: ThreadKey,
    pub(crate) text: String,
    pub(crate) message_id: String,
    pub(crate) subtitle: String,
    /// The explicit error-card retry signal (spec #391): this run re-submits a
    /// failed turn and replies a NEW card below the failed one (which the
    /// handler marks `Retried`). `Turn::start` then carries the failed
    /// attempt's render baseline across so the fresh card renders only the new
    /// attempt (#387). False for a fresh message.
    pub(crate) is_retry: bool,
    pub(crate) requester_open_id: Option<String>,
    pub(crate) is_group: bool,
    /// The id cola assigned to this turn's user message (`msg_cola_…`,
    /// ADR-0026). Set when this run re-submits a previous attempt (error-card
    /// retry) so the server deduplicates by id; None means a fresh message,
    /// and the turn generates a new id.
    pub(crate) cola_message_id: Option<String>,
    /// Downloaded images attached to this turn (Image Attachments).
    pub(crate) images: Vec<ImageAttachment>,
    /// The Backend's ADVISORY status read reported a live Execution while cola
    /// owned no live card chain (ADR-0062): the new Turn's card opens with
    /// [`MERGE_OPENING`], and its prompt still carries the merge delivery (the
    /// V2 steer), so a genuinely live run merges the message exactly as a
    /// Supplement would. False when the advisory read was idle or unreadable,
    /// and for a retry (never routed by this reading).
    pub(crate) advisory_live: bool,
}

/// One user→assistant exchange (GLOSSARY.md: Turn). Owns the per-turn state and
/// carries it through the phases in [`Turn::run`]:
///
/// - `start` — the busy guard, the Loading card, the fresh accumulator;
/// - `attempt` — one prompt send with the incremental renderer attached,
///   capturing the overrides at send time (ADR-0019);
/// - `recreate` — replace a stale session mapping and carry the live card,
///   inflight guard and cover title across (the remap C1 deferred);
/// - `finish` — first drain the render poll across a Supplement that missed
///   the run (ADR-0043), then reconcile the final parts, Turn Footer, cover
///   sync, group notice, and release the guard.
pub(crate) struct Turn {
    session_id: String,
    thread_key: ThreadKey,
    text: String,
    cola_message_id: String,
    images: Vec<ImageAttachment>,
    /// The session's working directory, captured at turn start — the routing
    /// key for the drain's `session_status` read (ADR-0010).
    directory: String,
    /// The variant the last attempt actually sent, captured at send time —
    /// what the Turn Footer shows.
    turn_variant: Option<String>,
    /// When cola started the turn (wall clock), for the long-task Completion
    /// Notice: a turn that ran past the threshold notifies on completion even
    /// in p2p (ADR-0043 amendment 2026-09-21).
    started_at: std::time::Instant,
    /// Whether the drain has observed the submitted run START (a Busy/Retry
    /// status, or a message the turn itself produced — a terminal finish, a
    /// recorded failure, or a step created at/after the anchor). The prompt
    /// submit only *schedules* execution (ADR-0056), so before this flips a
    /// non-busy status can just mean "not registered yet" — believing it would
    /// finalize the card Done before a single token arrived. A completed
    /// straddling step from a prior turn does NOT count: the transcript's
    /// membership rule includes a straddler that was still producing when this
    /// turn began, so `produced_a_step` filters it out by requiring a step
    /// CREATED at/after the anchor — without that filter a stale step could
    /// flip this.
    drain_started: bool,
    /// How many consecutive reads have seen no run yet. The admit only
    /// *schedules* execution, and the old confirmed-absence window is kept:
    /// after [`IDLE_CONFIRMATIONS`] unstarted reads the settle decision judges
    /// the submit — Complete when its message landed, Unreceived when it never
    /// did (ADR-0062) — instead of observing to the drain bound. Only
    /// consulted while `drain_started` is false.
    unstarted_reads: usize,
    /// Whether this turn's submit was REJECTED (the prompt call returned Err).
    /// A rejected submit may never have scheduled a run at all, so the drain
    /// must not mistake an idle status for a registration race and keep
    /// observing to its bound: the error is the outcome, and the drain settles
    /// on its first non-busy read.
    submit_failed: bool,
    /// The failure the drain last observed in the turn, for the case where the
    /// final reconcile's transcript read fails or has no anchor: the failure is
    /// then taken from this snapshot instead of silently stamping Done. Only
    /// ever a fallback — an authoritative final read (with the anchor) decides.
    last_drain_error: Option<String>,
    /// The transcript snapshot of the drain's most recent rendered read (#604).
    /// When the drain settled cleanly this is the read that was rendered AND
    /// showed no new content, and finalization decides the ending on it — never
    /// a later separate read that can disagree. `None` before the first drain
    /// read (a rejected submit or a vanished card).
    last_transcript: Option<SessionTranscript>,
    /// The stopped-finalization line is logged once per Turn (ADR-0048): both
    /// the post-prompt drain and its pre-finalization re-check observe the
    /// same sticky `/stop` marker, so the second observation must stay silent.
    stop_finalization_logged: bool,
}

impl Turn {
    /// Run one prompt end-to-end: `start` → `attempt` (→ `recreate` + `attempt`
    /// on a stale mapping) → `finish`. The lifecycle entry; the phases are
    /// internal seams, and the module's other surface is the card-delivery
    /// interface below.
    ///
    /// `handles` is the narrow bundle the coordinator built for this turn
    /// (spec #298, A1): sessions, cards, the request and wait state, the
    /// backend, the platform and the turn config. The Turn never sees the
    /// aggregate.
    ///
    /// The whole lifecycle runs inside a [`span::turn`], so every awaited
    /// Backend call inherits the session's fields (ADR-0048); the render poll
    /// runs on its own task and is instrumented where it is spawned
    /// ([`RenderPoll`]). A 404 recreate changes the session under the turn, so
    /// `run_inner` re-scopes the rest of the trace to the fresh session.
    pub(crate) async fn run(handles: &TurnHandles, ctx: PromptContext) -> crate::error::Result<()> {
        let span = span::turn(&ctx.session_id, &ctx.thread_key, tracing::Span::current().id());
        Self::run_inner(handles, ctx).instrument(span).await
    }

    /// The lifecycle body, wrapped by [`Turn::run`]'s span. A separate fn so
    /// the instrumentation wraps every phase — `start` included — and the span
    /// stays alive across each await.
    async fn run_inner(handles: &TurnHandles, ctx: PromptContext) -> crate::error::Result<()> {
        let Some(mut turn) = Self::start(handles, ctx).await? else {
            return Ok(()); // busy: start already answered
        };
        // The one INFO anchor per Turn (ADR-0048): session/chat/topic ride the
        // span prefix; the directory the turn works in and the prompt's size
        // are long data that belongs here — never the prompt body.
        tracing::info!(
            "turn start: dir={} prompt_chars={}",
            turn.directory,
            turn.text.chars().count()
        );
        let mut prompt_resp = turn.attempt(handles).await;
        // The mapped session may not exist on the current server — e.g. it was
        // created in an old, now-abandoned store. Clear the mapping, create a
        // fresh session and retry once.
        if prompt_resp.as_ref().is_err_and(|e| e.is_session_not_found()) {
            tracing::warn!("session {} not found on the server; recreating", turn.session_id);
            turn.recreate(handles).await?;
            // The Turn now works on the fresh session: re-scope the rest of its
            // trace so the retry, the finalization and the drain are
            // retrievable by the session they belong to (the stale id stays on
            // the lines above, including the warning that names what was
            // missing). Rooted and not re-recorded on the stale span: the fmt
            // layer appends a re-recorded field, which would print both ids on
            // every line.
            let span = span::turn(&turn.session_id, &turn.thread_key, None);
            prompt_resp = turn.attempt(handles).instrument(span.clone()).await;
            turn.finish(handles, &prompt_resp).instrument(span).await;
            return Ok(());
        }
        turn.finish(handles, &prompt_resp).await;
        Ok(())
    }

    /// The busy guard, the Loading card (a fresh reply below the user's
    /// message — including a retry, which replies a new card) and the fresh
    /// accumulator this turn streams into. `Ok(None)` means another prompt
    /// holds the session — already answered.
    async fn start(handles: &TurnHandles, ctx: PromptContext) -> crate::error::Result<Option<Turn>> {
        let PromptContext {
            session_id,
            thread_key,
            text,
            message_id,
            subtitle,
            is_retry,
            requester_open_id,
            is_group,
            cola_message_id,
            images,
            advisory_live,
        } = ctx;
        // This logical user message keeps ONE id across every attempt of this
        // turn (ADR-0026): a retry carries the previous attempt's id so the
        // server deduplicates; a fresh message generates a new one.
        let cola_message_id = cola_message_id.unwrap_or_else(crate::opencode::parsing::cola_message_id);
        // Serialize prompts per session: if one is already in flight, don't let
        // a second message overwrite its accumulator (the two would race on the
        // same card). Reply with a notice only when we own a fresh message.
        {
            let mut inflight = handles.waits.inflight.lock().await;
            if inflight.contains(&session_id) {
                drop(inflight);
                // The message was refused, not admitted: drop the inbound
                // claim with it (#424) — the inflight guard blocks the Fresh
                // path anyway, and a lingering claim would only outlive its
                // message.
                handles.waits.clear_inbound(&session_id).await;
                if is_retry {
                    // The retry lost the guard: nothing was submitted, so
                    // nothing may be marked. Its user message is the failed
                    // turn's and the running prompt already answered it; give
                    // the claim back so the still-Error card keeps a working
                    // retry button (spec #391).
                    Self::release_recovery_claim(&handles.cards, &session_id).await;
                } else {
                    let _ = handles
                        .platform
                        .reply_text(&message_id, "⏳ 上一条消息还在处理中，请稍等它完成后重发。")
                        .await;
                }
                return Ok(None);
            }
            inflight.insert(session_id.clone());
        }
        // This Turn is the inbound message's admission: the pre-guard claim
        // (#424) is superseded by the inflight guard it just became.
        handles.waits.clear_inbound(&session_id).await;
        // A fresh Turn supersedes any `/stop` from an earlier one: the drain
        // marker (ADR-0043) is per-session and sticky until the next turn, so
        // clearing it here keeps a past stop from silencing this turn's drain.
        handles.waits.stopped_sessions.lock().await.remove(&session_id);

        // A new Turn in the thread supersedes every waiting card it displaces
        // (ADR-0059, spec #405): a card sitting on 「⏳ 等待后台任务」 is
        // collected as 「⏳ 部分完成 · 已由新消息接管」 — terminal, no Completion
        // Notice — so no waiting card hangs forever while its background work
        // runs on. Runs before the new accumulator replaces the session's own
        // card (and under the inflight guard, so no Wake can race the collect).
        Self::collect_thread_waiting(&handles.cards, &handles.sessions, &thread_key).await;

        // A retry marks its failed card `Retried` before the fresh attempt
        // exists (spec #391): the card records that it was retried while the
        // new attempt lives on its own card below. Done only HERE — the
        // inflight guard is already held, so a retry that lost it can never
        // stamp 已重试 without submitting (the lost-guard path above released
        // the claim instead). The flush is best-effort: a failed marker PATCH
        // only warns, the retry proceeds.
        if is_retry {
            Self::mark_retried(&handles.cards, &session_id).await;
        }

        // Fresh accumulator per prompt: reuse leaks stale text/tools from the
        // previous turn into the next card. The card's IDENTITY (the message
        // id) carries over — only the content is reset.
        //
        // The CardSession is inserted BEFORE the loading-card round-trip (and
        // with `card_message_id: None`, filled in once the reply lands): a
        // Supplement that arrives while that request is in flight must find a
        // live card to split (ADR-0043). Only lock acquisitions separate the
        // busy guard from this insert — no I/O — so the card-less window is as
        // small as the two mutexes make it. The flush leaves the split pending
        // on a missing id, and the first flush after the id lands serves it.
        let started_at = std::time::Instant::now();
        let mut acc = StreamAccumulator::new(&subtitle);
        Self::seed_wake_floor(&handles.cards, &session_id, &mut acc);
        // The advisory-live opening line (ADR-0062): pushed BEFORE any part can
        // render, so it heads the card's timeline and every later flush keeps
        // it — the card that owns this message says the message merged into a
        // live run, whether the read was stale (this Turn runs it) or true
        // (the run merges it through the steer delivery).
        if advisory_live {
            acc.push_receipt(MERGE_OPENING);
        }
        acc.set_reply_target(Some(message_id.clone()));
        acc.set_session(&session_id);
        // The notice's clock travels on the card (ADR-0060): a quiet true end
        // is settled by Session Sync, which has no Turn to read `started_at`
        // from.
        acc.set_turn_started_at(started_at);
        // The id this turn's user message carries, so a later retry reuses
        // it (ADR-0026) — the server deduplicates by id.
        acc.set_cola_message_id(&cola_message_id);
        // Full original prompt, so the error-card "retry" can re-submit it.
        acc.set_prompt(&text);
        acc.set_requester(requester_open_id.clone());
        acc.set_is_group(is_group);
        // An explicit retry carries the failed attempt's render baseline
        // (#387, spec #391; ADR-0040's amendment): load-bearing on V1's
        // reused-id branch and on V2's fresh-id killed-run case (an admitted,
        // unfinished message can still fall inside the fresh anchor's in-flight
        // window); inert on the settled fresh-id case. A fresh prompt starts
        // clean.
        if is_retry {
            let live = handles.cards.cards.lock().await;
            if let Some(previous) = live.get(&session_id) {
                acc.carry_attempt_baseline(&previous.acc);
            }
        }
        // Instant Reminder (ADR-0043): register this turn's generation for
        // the Chat/Topic, so the pending-request pollers pin towards THIS
        // turn's requester and a stale clear from an earlier turn can never
        // unpin it. This is also where a pin orphaned by a crash or restart
        // is cleared once, on the Chat/Topic's next turn (self-healing).
        let generation = handles
            .waits
            .reminder
            .begin_turn(
                &handles.platform,
                &thread_key.chat_id,
                is_group,
                requester_open_id.as_deref(),
            )
            .await;
        acc.set_turn_generation(generation);
        {
            let mut cards = handles.cards.cards.lock().await;
            cards.insert(
                session_id.clone(),
                crate::bridge::turn::state::CardSession::new(acc, None),
            );
        }

        let mut loading = crate::feishu::card::shell::CardBuilder::new()
            .with_state(crate::feishu::card::CardState::Loading)
            .with_subtitle(&subtitle);
        if advisory_live {
            // The first visible card carries the same opening line the first
            // flush will re-render from the accumulator: the reply window can
            // outlast the render poll, and the card must not open silent.
            loading = loading.with_text(MERGE_OPENING);
        }
        let loading = loading.build();
        // Every attempt gets its own card, a retry included: the failed card
        // stays below the new one, marked `Retried` by the retry handler before
        // this submit (spec #391). The reply target is the failed turn's own
        // user message, so the new card lands directly below the failure.
        let new_card_id = match handles.cards.feishu.reply_card(&message_id, &loading).await {
            Ok(id) => id,
            Err(e) => {
                // The turn never started: drop the just-inserted card
                // session and release the guard, so nothing leaks and the
                // session does not look busy until a restart.
                handles.cards.cards.lock().await.remove(&session_id);
                release_inflight(handles, &session_id).await;
                return Err(e);
            }
        };

        // The mapped directory (a cheap store read), needed both by the durable
        // record's reap route (ADR-0063) and by the work context below.
        let session_dir = handles
            .sessions
            .entry_for_session(&session_id)
            .await
            .map(|e| e.directory.clone())
            .unwrap_or_default();
        // The loading card is the session's live card now: attach its identity
        // and persist the durable record (ADR-0063) — the anchor follows on the
        // first read that carries the submitted message. A previously recorded
        // orphan this Turn's card replaces is handed back uncollected, so the
        // takeover seed below can tell the collect whether it resolved the
        // orphan's running tail onto the successor (spec #561).
        let orphan = Self::take_over_card_deferring_collect(
            &handles.cards,
            &session_id,
            &new_card_id,
            Some(&session_dir),
        )
        .await;
        // The takeover seed (spec #561, ticket #565): resolve the orphaned
        // chain's Rendered Cursor against the session's own read and hand the
        // fresh card the orphan Turn's undelivered delta — its window, cut at
        // the confirmed frontier, plus the live-set calls resolved by identity
        // — before the prompt is submitted. A message that wins the race
        // against the adoption shows the run's final undelivered tail exactly
        // once, as a continuation; the collect follows the seed, so the orphan
        // card's running `⏳` panels leave its preserved body only when the seed
        // actually resolved them here (the Background Task Ledger leaves either
        // way: this card's own reads rebuild the live list).
        if let Some(orphan) = orphan {
            // A predecessor's pending orphan gap survives the takeover (spec
            // #561, review #569): the re-point carried it onto this card's
            // record, and the accumulator mirrors it so the first render read
            // that can place it lands it here. The seed below never invents a
            // gap over it (its own failure path defers to a carried one).
            if let Some(gap) = orphan.pending_gap.clone() {
                let mut live = handles.cards.cards.lock().await;
                if let Some(card) = live.get_mut(&session_id)
                    && card.card_message_id.as_deref() == Some(new_card_id.as_str())
                {
                    card.acc.set_pending_gap(gap);
                }
            }
            let resolved = Self::seed_orphan_delta(handles, &session_id, &new_card_id, &orphan).await;
            crate::bridge::chain::collect_orphan_after_takeover(
                &handles.cards,
                &session_id,
                &orphan.card_message_id,
                &resolved,
            )
            .await;
        }

        // The work context (ADR-0019) is captured before the prompt runs but
        // AFTER the card is live and its id known: the git read neither delays
        // the loading card nor keeps the session card-less, and it runs outside
        // the cards lock.
        let work_context = StreamAccumulator::capture_work_context(&session_dir).await;
        {
            let mut cards = handles.cards.cards.lock().await;
            if let Some(card) = cards.get_mut(&session_id) {
                card.acc.apply_work_context(work_context);
            }
        }

        // Instant Reminder (ADR-0043): the generation is registered so every
        // pending Permission/Question of this turn pins with it.
        Ok(Some(Turn {
            session_id,
            thread_key,
            text,
            cola_message_id,
            images,
            directory: session_dir,
            turn_variant: None,
            started_at,
            drain_started: false,
            unstarted_reads: 0,
            submit_failed: false,
            last_drain_error: None,
            last_transcript: None,
            stop_finalization_logged: false,
        }))
    }

    /// Send one attempt with the incremental renderer attached. The overrides
    /// are captured at send time (ADR-0019), and the renderer always stops
    /// before the submit returns so a retry starts its own cleanly. The
    /// submit itself does not wait for the turn (ADR-0056's submit+observe):
    /// the post-prompt drain observes the run it scheduled.
    async fn attempt(&mut self, handles: &TurnHandles) -> crate::error::Result<()> {
        let render = render::RenderPoll::spawn(handles, &self.session_id, &self.thread_key);
        // Capture the variant actually sent this turn AT SEND TIME, not at
        // finalization: a `/think` issued mid-generation must not retro-tag the
        // card of a turn that was sent without it (same "capture at turn start"
        // rule as the work-context 📁 half, ADR-0019). On a durable generation
        // (V2) the session's own selection is the source of truth — another
        // client may have changed it — so a stale mirror cannot tag the turn.
        self.turn_variant = handles
            .sessions
            .effective_variant(&handles.backend, &self.session_id, Some(&self.directory))
            .await;
        // The per-prompt axes: V1 sends these with the message, while a
        // generation that keeps a durable selection (V2) deliberately drops
        // them inside its strategy — the pick already lives on the session as
        // a switch. Do NOT "fix" a V2 pick by making the strategy re-send them.
        let model = handles.sessions.model_override(&self.session_id).await;
        let agent = handles.sessions.agent_override(&self.session_id).await;
        let prompt_resp = handles
            .backend
            .prompt(
                &self.session_id,
                &self.text,
                &image_inputs(&self.images),
                model.as_ref(),
                self.turn_variant.as_deref(),
                agent.as_deref(),
                Some(&self.cola_message_id),
            )
            .await;
        render.stop().await;
        // The poll runs only while the submit is in flight; the post-prompt
        // drain observes the submitted run from here on (ADR-0056, ADR-0043).
        // A rejected submit is recorded so the drain does not wait out its
        // bound for a run that was never scheduled.
        self.submit_failed = prompt_resp.is_err();
        prompt_resp
    }

    /// Replace the dead mapping with a fresh session on the current server —
    /// never fall through to another stale mapping for the thread. Reuses the
    /// dead session's directory so the user keeps working in the same project,
    /// keeps its topic creation messages (ADR-0023) so the injection guard
    /// survives, and carries the turn's live state (card accumulator, inflight
    /// guard, cover title) across to the fresh session.
    async fn recreate(&mut self, handles: &TurnHandles) -> crate::error::Result<()> {
        let flow = handles.flow();
        let old_entry = match flow.unmap(&self.session_id).await {
            Ok(entry) => entry,
            Err(e) => {
                release_inflight(handles, &self.session_id).await;
                return Err(e);
            }
        };
        let directory = old_entry
            .as_ref()
            .and_then(|e| (!e.directory.is_empty()).then_some(e.directory.clone()))
            .unwrap_or_else(|| handles.config.default_session_directory());
        let fresh_id = match flow
            .create_fresh_session(
                &self.thread_key,
                directory,
                old_entry.as_ref().and_then(|e| e.topic_anchor.clone()),
                old_entry.as_ref().and_then(|e| e.topic_root.clone()),
            )
            .await
        {
            Ok(id) => id,
            Err(e) => {
                // The dead mapping is gone but the guard still names the old
                // session: release it so the thread is not stuck busy.
                release_inflight(handles, &self.session_id).await;
                return Err(e);
            }
        };
        // Re-key the session's live card (accumulator + card identity in one
        // CardSession) from the dead session to the new one — a single remap
        // instead of three maps kept in lockstep. The accumulator's own
        // `session_id` travels too: the Error card's retry button is built from
        // it, so a post-recreate error must still offer a retry that resolves
        // against the fresh mapping.
        {
            let mut cards = handles.cards.cards.lock().await;
            if let Some(mut card) = cards.remove(&self.session_id) {
                card.acc.set_session(&fresh_id);
                cards.insert(fresh_id.clone(), card);
            }
        }
        // The live-card record is a fact about the card, not the session id it
        // was filed under: re-key it so the fresh session's card is reap-able.
        handles.cards.chains.rename(&self.session_id, &fresh_id);
        {
            let mut inflight = handles.waits.inflight.lock().await;
            inflight.remove(&self.session_id);
            inflight.insert(fresh_id.clone());
        }
        // The cover card belongs to the topic, not the dead session — move its
        // recorded title to the fresh session so the title-sync hook keeps
        // patching the card after the recreate (ADR-0023).
        {
            let mut covers = handles.cards.cover_titles.lock().await;
            if let Some(cover) = covers.remove(&self.session_id) {
                covers.insert(fresh_id.clone(), cover);
            }
        }
        self.session_id = fresh_id;
        Ok(())
    }

    /// Reconcile the final parts the incremental poll may have missed, fill in
    /// the Turn Footer's model/context data, sync the topic cover, send the
    /// group completion notice, and release the busy guard.
    ///
    /// Finalization begins with the post-prompt drain (ADR-0043): a Supplement
    /// that missed the running run starts a new Turn on the server, and the
    /// drain keeps the render poll alive (still holding the inflight guard, so
    /// a further message is treated as a Supplement) until the run reaches its
    /// true end — then the card is marked with the decided ending and the guard
    /// released. The drain is UNBOUNDED (#603): there is no total budget and no
    /// hand-off; the only ceilings are the lost-contact and stuck-panel graces,
    /// which end a run nobody can act on.
    async fn finish(&mut self, handles: &TurnHandles, prompt_resp: &crate::error::Result<()>) {
        // Post-prompt drain + finalization, as ONE loop (#603, #604). The drain
        // settles only on a rendered, quiescent read; finalization then makes
        // ONE fresh confirmation read. Content that landed after the settle but
        // before that read — the run went live again — is rendered and the drain
        // is RE-OPENED, so the run is followed to its true end by the drain's own
        // settle logic and its graces (no total budget). There is no path that
        // stamps an ending on a read that still shows unrendered new content. A
        // decided (grace) ending's disposition is already final: it reads at most
        // once and stamps.
        //
        // The drain's exit is not the last word: the pre-finalization re-check
        // drains a Supplement racing the exit rather than dropping it, and one
        // that lands after the release becomes a normal new Turn on the
        // handler's not-busy path.
        let (drain_outcome, final_transcript) = loop {
            let drain_outcome = self.drain_after_prompt(handles).await;

            // The accumulator vanished under the drain (a replacement or a
            // collect took the card): the Turn owns nothing any more, so it ends
            // silently — no ending is stamped and no notice is sent, exactly as
            // the out-of-turn follow does when its render finds no accumulator
            // (review, PR #595).
            if drain_outcome == Some(DrainState::Vanished) {
                tracing::info!("turn drain: card vanished on session {}", self.session_id);
                self.release(handles).await;
                return;
            }

            // A decided ending — the waiting yield, the Unreceived ending, the
            // two graces: its disposition is final, so finalization reads at most
            // once (for any tail the grace's read carried) and stamps it.
            if drain_outcome.is_some() {
                break (drain_outcome, self.final_read_once(handles).await);
            }

            // A settled drain: one fresh read confirms it. Quiescent → decide on
            // it. Still carrying content the card lacks → the run went live
            // again: render it and re-open the drain, never stamping an ending
            // on this non-quiescent read.
            match self.settled_final_read(handles).await {
                SettledFinalRead::Quiescent(transcript) => break (drain_outcome, Some(*transcript)),
                SettledFinalRead::Unreadable => break (drain_outcome, self.last_transcript.clone()),
                SettledFinalRead::Live => {
                    tracing::info!(
                        "turn drain: content still arriving after settle on session {}; re-opening the drain",
                        self.session_id
                    );
                }
            }
        };

        // A deliberate `/stop` owns this turn's ending (#394): the abort the
        // server recorded is NOT a failure, its text must never reach the
        // card, and the card finalizes `Stopped`. The marker is sticky until
        // the next Turn's `start` clears it. Read it AFTER the transcript read:
        // that read is an await, so a stop landing during it must still win.
        let mut stopped = handles.waits.is_stopped(&self.session_id).await;
        let prompt_err = match prompt_resp {
            Err(e) => Some(e.to_string()),
            Ok(()) => {
                let observed = final_transcript
                    .as_ref()
                    .and_then(|transcript| self.turn_error(transcript));
                if observed.is_some() {
                    observed
                } else if final_transcript
                    .as_ref()
                    .is_some_and(|transcript| transcript.anchor_of_user(&self.cola_message_id).is_some())
                {
                    // An authoritative final read (the anchor is present) says
                    // the newest assistant message is clean: no failure.
                    None
                } else {
                    // A failed read, or one that does not carry the anchor yet:
                    // keep the failure the drain observed — never Done by a
                    // hiccup.
                    self.last_drain_error.clone()
                }
            }
        };

        // The single settle decision (ADR-0059, ADR-0062): a Turn that idles
        // with live Background Tasks yields waiting — never ✅, never a
        // terminal, no Completion Notice — and one whose submitted message
        // never landed at an idle session ends Unreceived, also never ✅. The
        // drain's own ending decides; the final reconcile read re-decides it
        // so a task that appeared, the retirement that arrived, or the message
        // that finally landed inside this finalization window is honored: an
        // undecided read (a Wake whose Execution has not reached its boundary)
        // keeps the drain's own disposition, and a stop or a failure dominates
        // below.
        // The settle scope: the final read's own anchor when it still carries
        // the submitted message, else the card's captured anchor — the anchor
        // is sticky (a compaction or a partial read cannot un-land a message
        // the Turn already anchored), so Unreceived is decided only when
        // neither knows the message.
        let captured_anchor = Turn::armed_turn_anchor(&handles.cards, &self.session_id).await;
        let final_settle = final_transcript.as_ref().map(|transcript| {
            let anchor = transcript
                .anchor_of_user(&self.cola_message_id)
                .or_else(|| captured_anchor.clone());
            transcript.settle(anchor.as_ref())
        });
        // The ending the drain already decided outranks a later re-decision:
        // its two graces (#603) are loop-only endings the settle table cannot
        // produce, so they pass straight through. Every other case reads the
        // settle decision through the one disposition table: a decided read is
        // its disposition; an undecided one (a Wake's Execution has not reached
        // its boundary yet, or the read failed) keeps the drain's own last word
        // — Waiting or Unreceived when that found one, else no ending yet
        // (Observe).
        let ending = match drain_outcome {
            Some(DrainState::LostContact) => Disposition::LostContact,
            Some(DrainState::StuckPanel) => Disposition::StuckPanel,
            _ => match final_settle {
                Some(TurnSettle::Running) | None => match drain_outcome {
                    Some(DrainState::Waiting) => Disposition::Waiting,
                    Some(DrainState::Unreceived) => Disposition::Unreceived,
                    _ => Disposition::Observe,
                },
                Some(settle) => Disposition::from(settle),
            },
        };
        // The ending this finalization applies, in its precedence: a
        // deliberate `/stop` dominates every other signal (#394) — applied
        // when the marker is re-read below — then a recorded failure (the
        // prompt's `Err`, the transcript's turn error, or the failure the
        // drain last observed: `prompt_err` above), then the settle decision's
        // Waiting/Unreceived ending, else the true end. A stop or a failure
        // suppresses the yield, exactly as the replaced branches' guard did.
        let mut disposition = match &prompt_err {
            Some(error) => Disposition::Failed(error.clone()),
            None => match ending {
                Disposition::Waiting => Disposition::Waiting,
                Disposition::Unreceived => Disposition::Unreceived,
                // The merged loop's own graces: a wedged Backend and an
                // unreconcilable panel end the card Error, never a false ✅.
                Disposition::LostContact => Disposition::LostContact,
                Disposition::StuckPanel => Disposition::StuckPanel,
                // No ending yet (the undecided read), the true end, a decided
                // `Failed` whose message did not surface through `prompt_err`
                // (the same projection: the final read lost the anchor), and
                // the loop-only `Stopped` (set below from the sticky marker)
                // all finalize Done here.
                Disposition::Observe | Disposition::Done | Disposition::Failed(_) => Disposition::Done,
                Disposition::Stopped => Disposition::Done,
            },
        };

        // #187: a turn that ended without completing (abort, interrupt, prompt
        // error) leaves every still-pending Permission/Question behind with a
        // dead tool fiber. Reject them here, so their blocks become `🚫 已拒绝`
        // receipts on the card instead of ghosting onto the next turn (#177).
        // Only an unfinished turn does this: a completed one has nothing
        // pending, and a request that outlived a healthy turn (external or
        // concurrent work) is not this turn's to deny. A stopped turn is
        // unfinished in exactly this sense — its abort strands whatever was
        // pending — even when the stop left no error text (#394).
        if stopped || prompt_err.is_some() {
            crate::bridge::request::flow::reject_leftovers_for_turn(
                &handles.requests,
                &handles.cards,
                &handles.sessions,
                &handles.backend,
                &self.session_id,
            )
            .await;
        }

        // The Turn Footer's variant is a fact of THIS turn (captured at send
        // time, ADR-0019). The model/token halves are captured from the
        // transcript's messages themselves.
        if let Some(card) = handles.cards.cards.lock().await.get_mut(&self.session_id) {
            card.acc.set_variant(self.turn_variant.clone());
        }

        // Reconcile: render any parts the incremental poll missed from the
        // settled transcript read above, then apply this Turn's ending.
        //
        // A stop landing after the read above (during the leftover rejection,
        // or any scheduler hop since) must still win: re-read the marker
        // immediately before the stamp. The residual window is the cards-lock
        // acquisition below; a stop landing after it is indistinguishable from
        // one landing just after a completed turn, and the next Turn owns it.
        stopped = stopped || handles.waits.is_stopped(&self.session_id).await;
        if stopped {
            // The stop is this Turn's ending, whatever the settle decision
            // said (#394): it discards a recorded failure and the card takes
            // its own terminal.
            disposition = Disposition::Stopped;
        }
        // The completion entries this final read owes (spec #588, #593): plan
        // them under a brief read of the card, spend each entry's one output
        // read OUTSIDE the cards lock (the reads are network), and commit the
        // surviving plan with the final render below — the same plan/read/commit
        // seam the live path uses, so a shell completion first seen here carries
        // its tail (or 「输出已不可用」) instead of a bare entry. The plan is
        // pure and the commit's announce gate keeps the entry exactly once.
        let mut plans = Vec::new();
        if let Some(transcript) = &final_transcript {
            let mut cards = handles.cards.cards.lock().await;
            if let Some(card) = cards.get_mut(&self.session_id) {
                plans = render::plan_finalization_entries(&mut card.acc, transcript);
            }
        }
        render::read_planned_outputs(&handles.backend, &mut plans).await;
        {
            let mut cards = handles.cards.cards.lock().await;
            if let Some(card) = cards.get_mut(&self.session_id) {
                let acc = &mut card.acc;
                if let Some(transcript) = &final_transcript {
                    render::render_new_turn_parts_committing(acc, transcript, plans);
                }
                // Capture the answering model + token usage from the LATEST
                // assistant message unconditionally — the render dedup may have
                // captured them before the message carried its final tokens.
                // Usage only when nonzero: an in-flight step's message carries
                // zeros and must not wipe the last completed step's figure.
                if let Some(transcript) = &final_transcript {
                    let latest_assistant = transcript
                        .messages
                        .iter()
                        .rfind(|message| message.role == MessageRole::Assistant);
                    if let Some(message) = latest_assistant {
                        render::capture_footer_model(acc, message);
                        if let Some(tokens) = &message.tokens {
                            let used = tokens.context_used();
                            if used > 0 {
                                acc.set_context_tokens(used);
                            }
                        }
                    }
                }
                // The one ending application every path shares (spec #538):
                // the card's state, failure line and phase timer all come from
                // the disposition decided above — a stop discards a recorded
                // failure, a failure records its own line, and the yield / true
                // end leave it as it was.
                acc.apply_ending(&disposition);
                tracing::info!(
                    "final render: fetched_messages={} text={} reasoning={} tools={} rendered_parts={} error={}",
                    final_transcript
                        .as_ref()
                        .map(|transcript| transcript.messages.len())
                        .unwrap_or(0),
                    acc.text().len(),
                    acc.reasoning().len(),
                    acc.tools.len(),
                    acc.rendered_parts.len(),
                    acc.error().unwrap_or("none"),
                );
            }
        }
        // Refresh the Turn Footer's work context at turn end (ADR-0019): the AI
        // may have created or switched branches, or committed, so re-read the
        // git state before the final flush — the footer shows where the turn
        // landed, not just where it started.
        crate::bridge::turn::state::refresh_work_context(&handles.cards, &self.session_id).await;
        // Refresh the Turn Footer's context window (ADR-0044) before the final
        // flush: the render-poll refresh usually covered it, but the reconcile
        // above may have just captured a final usage the polls never saw. Runs
        // on a failed prompt too — the card already carries that usage.
        crate::bridge::turn::state::refresh_context_window(
            &handles.cards,
            &handles.backend,
            &self.session_id,
        )
        .await;
        Self::flush_card(&handles.cards, &self.session_id).await;
        // The identity of the card this Turn's ending actually landed on (spec
        // #602, review finding 6; ticket #607): captured AFTER the ending flush,
        // so a size split that moved the terminal slice onto a continuation
        // names the continuation — the card carrying the ending — not the
        // finalized prefix the split replaced (which would read as a "newer
        // Turn replaced the card" and suppress the notice). A genuine NEW Turn
        // that swaps the card in the awaits below still differs from it, so the
        // notice stays bound to the card the ending landed on. The ending's
        // fate and the recipient are read from this same post-flush card under
        // one lock inside `announce_completion`.
        let stamped_card = handles
            .cards
            .cards
            .lock()
            .await
            .get(&self.session_id)
            .and_then(|card| card.card_message_id.clone());

        // Topic cover card (ADR-0023): once the server holds a real title for
        // the session — auto-generated after the first exchange, or set by
        // `/name` — patch the cover card (the thread root) in place, so the
        // chat-list topic entry stays current. Best effort; failures only log.
        if prompt_err.is_none() {
            let settled = crate::bridge::topic::sync_topic_cover_title(
                &handles.cards,
                &handles.sessions,
                &handles.backend,
                &self.session_id,
            )
            .await;
            // The auto-title can still be in flight when a short turn ends
            // (the title agent races the turn); only then retry with backoff,
            // so the cover follows even if the user stops here (ADR-0023).
            if !settled {
                crate::bridge::topic::spawn_cover_title_retry(
                    &handles.cards,
                    &handles.sessions,
                    &handles.backend,
                    &self.session_id,
                );
            }
        }

        // No external-sync baseline is recorded here (ADR-0026): authorship is
        // carried by this turn's `msg_cola_` message id, and the external-message
        // poller alone owns the Sync Watermark, so a failed/errored turn can no
        // longer leave a stale watermark that makes cola's own message look
        // external after a server heal.

        // Completion notice (ADR-0043 amendment 2026-09-21): the streaming
        // card is patched in place, which pushes no notification and does not
        // bump the conversation — so reply to the requester's message to
        // notify them. The disposition's own classification declines a Waiting
        // yield and an Unreceived ending, so neither notifies — the notice
        // belongs to the true end (ADR-0059). `announce_completion` gates the
        // notice on the terminal write's own acceptance (#607).
        announce_completion(
            &handles.cards,
            &handles.platform,
            &handles.config.notice_rules(),
            &self.session_id,
            self.started_at,
            &disposition,
            stamped_card.as_deref(),
        )
        .await;

        // The guard is released at the true end. The drain ran to its end on
        // this very task, so there is no ownership hand-off and no guard-free
        // gap by construction: `busy()` (the server-yield read) never sees a
        // mid-request Session as idle.
        self.release(handles).await;
        // Permissions are handled by the independent poller spawned in App::run,
        // so a run waiting on a permission still gets its card shown.
    }

    /// Release this turn's busy guard. Idempotent.
    async fn release(&self, handles: &TurnHandles) {
        release_inflight(handles, &self.session_id).await;
    }

    /// The post-submit phase (ADR-0043): keep the renderer polling while the
    /// session is still running or an unanswered cola-authored Supplement is
    /// newer than this turn's anchor — a Supplement that missed the running
    /// run starts a new Turn on the Backend, and its reply must land on the
    /// live (continuation) card instead of nowhere.
    ///
    /// The drain's exit is not the last word: finalization re-checks once more
    /// BEFORE the card is marked with its ending and the guard released, so a
    /// Supplement racing a settled drain's exit is drained rather than dropped.
    /// A Supplement the re-check finds is drained again; a Supplement that
    /// lands after the release is seen by the handler's not-busy path and
    /// becomes a normal new Turn.
    ///
    /// Returns `Some(Waiting)` when the settle decision ended the drain with
    /// live Background Tasks, `Some(Unreceived)` when it found the submitted
    /// message never landed at an idle session (ADR-0059, ADR-0062), and
    /// `Some(LostContact)` / `Some(StuckPanel)` when the merged loop's graces
    /// ended a run nobody can act on (#603). `None` when the drain settled or
    /// stopped. `finish` maps each into the card's own ending.
    async fn drain_after_prompt(&mut self, handles: &TurnHandles) -> Option<DrainState> {
        loop {
            let last = self.drain(handles).await;
            // A decided ending — the waiting yield, the Unreceived ending and
            // the two graces — is final: no re-check may reopen the drain (its
            // render would add content to a card that is about to stop
            // updating, and the ending was already decided).
            if matches!(
                last,
                Some(
                    DrainState::Waiting
                        | DrainState::Unreceived
                        | DrainState::LostContact
                        | DrainState::StuckPanel
                        | DrainState::Vanished
                )
            ) {
                return last;
            }
            // The drain settled: one more tick must confirm it. A Supplement
            // racing the exit is drained; an undecided (Running) read — a
            // Wake's Execution not yet bounded, or a session that reads live —
            // reopens the drain, so the ending is never decided from a read a
            // later read contradicts (the same-snapshot rule's spirit). A
            // failed re-check read leaves the settled decision standing.
            // The re-check only CORROBORATES the settled decision: `drain()`
            // returns `None` (settled) solely from `Some(DrainState::Settled)
            // if tick.contact` above — the ending already rests on a full read
            // pair. So neither arm of this match needs its own `contact` check:
            // `Some(Settled)` is the same verdict re-observed, `None` is a
            // failed re-check read (NOT an unreadable-status decision — that
            // never returns `None` here), and a failed re-check deliberately
            // leaves the settled decision standing. Adding a `contact` check to
            // the `None` arm would let a transient read failure discard a valid
            // Done and hand it to the lost-contact grace — a harmful behavior
            // change, so the false positive is documented, not "fixed" (spec
            // #602 review, finding A / pre-push finding 4).
            match self
                .drain_tick(handles, self.drain_read_timeout_ms(handles))
                .await
                .state
            {
                Some(DrainState::Settled) | None => return last,
                Some(DrainState::Supplement) => {
                    tracing::info!(
                        "turn drain: supplement racing the finish on session {}; draining",
                        self.session_id
                    );
                }
                Some(_) => {}
            }
        }
    }

    /// The per-read bound the merged unbounded drain uses for every Backend
    /// read (the follow's one fixed read timeout, #386/#603): a hung Backend
    /// fails fast per call instead of freezing a tick.
    fn drain_read_timeout_ms(&self, handles: &TurnHandles) -> u64 {
        handles.config.follow_read_timeout_ms()
    }

    /// One bounded read finalization decides on (#603/#604). It uses the FIXED
    /// per-read timeout (spec #602 review, round 7): a grace-capped read
    /// (`min(follow_read_timeout_ms, grace)`) would misread a reachable Backend
    /// that answers within the read timeout but after a small grace as
    /// "unreadable", finalizing on the earlier snapshot before a late tail
    /// renders — the #604 race. The flow stays bounded because each read is
    /// bounded by `follow_read_timeout_ms` and no read is started after the
    /// drain's own graces have decided the ending; a read is never shortened to
    /// fit the grace.
    async fn finalization_read(&self, handles: &TurnHandles) -> Option<SessionTranscript> {
        crate::bridge::bounded_call(
            "turn final transcript",
            self.drain_read_timeout_ms(handles),
            handles.backend.transcript(&self.session_id),
        )
        .await
        .and_then(std::result::Result::ok)
    }

    /// The read a DECIDED ending finalizes on (#603): its disposition is already
    /// final, so one bounded read renders any tail the grace's read carried and
    /// the read is returned for the final reconcile. `None` when the read fails
    /// (the failure the drain last observed stands in).
    async fn final_read_once(&self, handles: &TurnHandles) -> Option<SessionTranscript> {
        let transcript = self.finalization_read(handles).await?;
        let _ = render::render_and_flush(
            &handles.cards,
            &handles.sessions,
            &handles.backend,
            &handles.requests,
            &self.session_id,
            &transcript,
        )
        .await;
        Some(transcript)
    }

    /// The settled drain's ONE fresh read (#604). When it shows nothing the card
    /// lacks it IS the rendered, quiescent snapshot the ending is decided on. A
    /// read that still carries content the card lacks means the run went live
    /// again: the read is rendered and the caller RE-OPENS the drain, so the
    /// ending is never stamped on a non-quiescent read. A failed read falls back
    /// to the drain's last rendered snapshot (a transient hiccup must not turn a
    /// settled ending into an unknown one).
    async fn settled_final_read(&self, handles: &TurnHandles) -> SettledFinalRead {
        let Some(transcript) = self.finalization_read(handles).await else {
            return SettledFinalRead::Unreadable;
        };
        // A read the card already shows: the rendered, quiescent snapshot to
        // decide on — no render needed (it would re-spend the cycle's ledger
        // reads and re-persist the chain record for a body that has not moved;
        // the drain's own early-return makes the same call).
        if !self.drain_owes_render(handles, &transcript).await {
            return SettledFinalRead::Quiescent(Box::new(transcript));
        }
        // The read carries content the card lacks: render it (the fresh RENDERED
        // read), then report Live so the caller re-opens the drain. A vanished
        // accumulator leaves the read as the snapshot it last saw.
        let Some(rendered) = render::render_and_flush(
            &handles.cards,
            &handles.sessions,
            &handles.backend,
            &handles.requests,
            &self.session_id,
            &transcript,
        )
        .await
        else {
            return SettledFinalRead::Quiescent(Box::new(transcript));
        };
        if rendered.stats.new_content {
            SettledFinalRead::Live
        } else {
            SettledFinalRead::Quiescent(Box::new(transcript))
        }
    }

    /// Poll the Backend and render it into the live card until the run reaches
    /// its true end. UNBOUNDED (#603): there is no total budget — a readable
    /// run may take as long as it takes. The only ceilings are the two graces
    /// for states nobody can act on: **lost contact** (no tick in the grace
    /// produced a full read pair) and **stuck panel** (a readable, settled card
    /// still carries a live `⏳`). A failed transcript read while a run was
    /// already observed pending is retried; every read is bounded by the one
    /// per-read timeout so a hung Backend fails fast.
    ///
    /// `Some(Waiting)`/`Some(Unreceived)` are the settle decision's endings;
    /// `Some(LostContact)`/`Some(StuckPanel)` are the two graces; `None` means
    /// the drain settled or stopped.
    async fn drain(&mut self, handles: &TurnHandles) -> Option<DrainState> {
        let poll_ms = handles.config.render_poll_ms();
        let read_timeout_ms = self.drain_read_timeout_ms(handles);
        let grace = std::time::Duration::from_millis(handles.config.follow_grace_ms());
        let hint_at = self.started_at + grace;
        let mut last_contact = tokio::time::Instant::now();
        let mut stuck_since: Option<tokio::time::Instant> = None;
        loop {
            // The first check runs before any sleep: a Supplement that missed
            // the run is already on the Backend when the submit returns.
            let tick = self.drain_tick(handles, read_timeout_ms).await;
            // The waiting hint (ADR-0062), folded in from the unreceived watch
            // (#603): while the run reads live but the submitted message has
            // not landed, the card gains the neutral line once the grace has
            // passed since the turn started — a genuine long tool call and a
            // dead run look identical from outside, so cola does not nag early.
            // Pushed once, then flushed.
            if tick.live
                && self.card_turn_anchor(handles).await.is_none()
                && std::time::Instant::now() >= hint_at
                && Turn::show_receive_hint(&handles.cards, &self.session_id).await
            {
                Turn::flush_card(&handles.cards, &self.session_id).await;
            }
            // The stuck-panel grace (#386): a readable, settled card still
            // carrying a live `⏳` panel must not be stamped Done over it — give
            // the panel the grace to settle, then end Error.
            if tick.settled_readable && Turn::has_live_tools(&handles.cards, &self.session_id).await {
                let since = *stuck_since.get_or_insert_with(tokio::time::Instant::now);
                if since.elapsed() >= grace {
                    tracing::info!("turn drain: unreconcilable panel on session {}", self.session_id);
                    return Some(DrainState::StuckPanel);
                }
            } else {
                stuck_since = None;
                match tick.state {
                    // A Settled ending may only finalize on a FULL read pair
                    // (spec #602, review): a settled state read out of an
                    // unreadable status (`contact == false`) is not evidence the
                    // run ended — the transcript's Complete alone cannot prove
                    // the Session idle, and finalizing on it would stamp a
                    // partial turn (a live `⏳` panel included) `✅` while
                    // `session_status` never answered, skipping the lost-contact
                    // grace below. Keep observing: the grace ends the card in
                    // error, or a later readable tick settles it. The `/stop`
                    // ending arrives with `contact == true`, so it still
                    // finalizes promptly.
                    Some(DrainState::Settled) if tick.contact => return None,
                    Some(DrainState::Settled) => {}
                    // The card vanished (a replacement or a collect): the Turn
                    // owns nothing any more, so stop silently.
                    Some(DrainState::Vanished) => return Some(DrainState::Vanished),
                    // The Turn yields waiting (ADR-0059) or ends Unreceived
                    // (ADR-0062): the ending is decided, so there is nothing
                    // left to observe — but only on a FULL read pair (spec
                    // #602, review): a `Waiting`/`Unreceived` read (just like a
                    // `Settled` one above) read out of an unreadable status
                    // (`contact == false`) comes only from the transcript, which
                    // cannot prove the Session's state. Acting on it would end
                    // the run *before* the lost-contact grace, contradicting
                    // #603's "a lost-contact run still ends in error at the
                    // grace". Keep observing: the grace ends the card in error,
                    // or a later readable tick yields. A readable non-live
                    // status still yields promptly.
                    Some(state @ (DrainState::Waiting | DrainState::Unreceived)) if tick.contact => {
                        return Some(state);
                    }
                    Some(DrainState::Waiting | DrainState::Unreceived) => {}
                    _ => {}
                }
            }
            // The lost-contact ceiling (#386): a tick that did not answer both
            // reads is no contact. A Settled/ending decision above still wins
            // when it came from a full read pair (contact == true); a decision
            // read out of an unreadable status never returns above (spec #602,
            // review), so this grace owns that state — a hung status cannot
            // hold the card forever, and it cannot short-circuit the grace.
            if tick.contact {
                last_contact = tokio::time::Instant::now();
            } else if last_contact.elapsed() >= grace {
                tracing::info!("turn drain: lost contact on session {}", self.session_id);
                return Some(DrainState::LostContact);
            }
            tokio::time::sleep(std::time::Duration::from_millis(poll_ms)).await;
        }
    }

    /// The live card's armed Turn anchor, if any — the merged drain's "the
    /// submitted message has landed" fact (the hint is owed only while it has
    /// not).
    async fn card_turn_anchor(&self, handles: &TurnHandles) -> Option<TurnAnchor> {
        Turn::armed_turn_anchor(&handles.cards, &self.session_id).await
    }

    /// One drain tick: read the Backend, decide whether rendering must go on,
    /// and render the very snapshot the decision was made from, so the live
    /// card follows the new Turn. The returned [`DrainTick`] carries `None` for
    /// its state when the transcript read failed (unknown state).
    async fn drain_tick(&mut self, handles: &TurnHandles, timeout_ms: u64) -> DrainTick {
        let Some(mut transcript) = self.drain_transcript(handles, timeout_ms).await else {
            return DrainTick {
                state: None,
                live: false,
                settled_readable: false,
                contact: false,
            };
        };
        // The shared Background Task runtime reconciliation (#589), on this
        // tick's own read and BEFORE the settle decision below: a task the
        // runtime confirms ended leaves the live list here, so the decision
        // sees the true end — the turn settles ✅ directly instead of yielding
        // 「⏳ 等待后台任务」 until a later Session Sync pass. It observes only
        // while the card can render the entry: the tick's render below needs
        // the Turn anchor (its own or the one this read carries) to place it,
        // and observing without one would record the task in the overlay with
        // no entry left to render. Zero requests when the read lists no live
        // task; a failed or timed-out read changes nothing.
        // The pass is applied to the transcript only (review, PR #595): it is
        // committed below, after the render that carries its entries lands.
        let pass = if self.drain_anchor_visible(handles, &transcript).await {
            handles
                .runtime_reconcile
                .observe(
                    &handles.backend,
                    &self.session_id,
                    &self.directory,
                    &mut transcript,
                    timeout_ms,
                )
                .await
        } else {
            None
        };
        let read = self.drain_state(handles, &transcript, timeout_ms).await;
        // A settled read whose content is already on the card owes no render:
        // it is the snapshot the ending is decided from — rendered, and showing
        // no new content — so the drain settles without another write. Rendering
        // it anyway would re-spend the cycle's ledger reads and re-persist the
        // chain record for nothing.
        if read.state == DrainState::Settled
            && !pass.as_ref().is_some_and(|pass| pass.changed())
            && !Turn::has_live_tools(&handles.cards, &self.session_id).await
            && !self.drain_owes_render(handles, &transcript).await
        {
            self.last_transcript = Some(transcript);
            return DrainTick {
                state: Some(read.state),
                live: read.live,
                settled_readable: read.settled_readable,
                contact: read.contact,
            };
        }
        // A settle whose tick has a retiring task must flush so the pass can
        // commit; a settle that lands nothing but turn parts leaves the card
        // write to finalization's own render on this same read (#604), so the
        // settle tick adds no second durable write and no out-of-order flush.
        let settling = read.state == DrainState::Settled && !pass.as_ref().is_some_and(|pass| pass.changed());
        let rendered = if settling {
            render::render_and_flush_settling(
                &handles.cards,
                &handles.sessions,
                &handles.backend,
                &handles.requests,
                &self.session_id,
                &transcript,
            )
            .await
        } else {
            render::render_and_flush(
                &handles.cards,
                &handles.sessions,
                &handles.backend,
                &handles.requests,
                &self.session_id,
                &transcript,
            )
            .await
        };
        let Some(rendered) = rendered else {
            // The accumulator vanished under the tick (a replacement or a
            // collect took the card): nothing is owned any more, so the drain
            // ends silently rather than recording a task the card cannot carry
            // (review, PR #595).
            return DrainTick {
                state: Some(DrainState::Vanished),
                live: read.live,
                settled_readable: false,
                contact: read.contact,
            };
        };
        // The record-after-flush invariant (review, PR #595): the pass commits
        // only once the render that carried its entries returned an accumulator
        // it wrote to AND the flush that carried them was accepted — delivered
        // now, or owed by the delivery layer's retry (ADR-0067). A permanently
        // refused write records nothing: the task stays live and the next live
        // card renders its entry.
        if rendered.flush.accepted()
            && let Some(pass) = pass
        {
            pass.commit(&handles.backend, &self.session_id, &transcript);
        }
        // The same-snapshot ending rule (#604): the TRUE end is only real when
        // the read it was decided from was rendered AND showed no new content.
        // If this read still carried parts the card lacked it was not quiescent
        // — report `Running` so the drain re-reads and re-decides on a fresh
        // snapshot instead of finalizing a read a later one contradicts. A
        // waiting yield or an Unreceived ending owns its own card already (the
        // render above landed anything new), so only the true end gates; a
        // retirement entry is content too, but its settle decision was made on
        // the same transcript, so it does not gate.
        let state = if read.state == DrainState::Settled && rendered.stats.new_content {
            DrainState::Running
        } else {
            read.state
        };
        // Keep the read finalization will decide on: when the drain settled this
        // is the rendered, quiescent snapshot, so `finish` never needs a later,
        // disagreeing read (#604).
        self.last_transcript = Some(transcript);
        DrainTick {
            state: Some(state),
            live: read.live,
            settled_readable: read.settled_readable,
            contact: read.contact,
        }
    }

    /// Whether a settled read still carries turn parts the card lacks — the
    /// pure probe ([`render::renders_new_content`]) that decides whether the
    /// settled tick must render before it may settle (#604). Only the turn's own
    /// window is covered: seeded/gap content the probe cannot place is rendered
    /// by finalization's own reconcile.
    async fn drain_owes_render(&self, handles: &TurnHandles, transcript: &SessionTranscript) -> bool {
        let mut cards = handles.cards.cards.lock().await;
        match cards.get_mut(&self.session_id) {
            Some(card) => {
                render::capture_turn_anchor(&mut card.acc, transcript);
                match card.acc.turn_anchor.clone() {
                    Some(anchor) => render::renders_new_content(&card.acc, transcript, &anchor),
                    None => false,
                }
            }
            None => false,
        }
    }

    /// Whether this drain tick may spend the runtime reconciliation: the card
    /// can render the retirement entry the read would produce. That is exactly
    /// "the accumulator has the Turn's anchor, or this read carries the
    /// submitted message" — [`render::capture_turn_anchor`] takes the anchor
    /// from that message on the render below. An anchorless (or card-less)
    /// tick is on its way to the Unreceived ending, where a recorded
    /// retirement would be swallowed by the overlay with no entry rendered;
    /// observing resumes on the first tick whose read is anchored.
    async fn drain_anchor_visible(&self, handles: &TurnHandles, transcript: &SessionTranscript) -> bool {
        let mut cards = handles.cards.cards.lock().await;
        match cards.get_mut(&self.session_id) {
            Some(card) => {
                render::capture_turn_anchor(&mut card.acc, transcript);
                card.acc.turn_anchor.is_some()
            }
            None => false,
        }
    }

    /// What the drain must do next (ADR-0043, ADR-0056): keep going while the
    /// session's run is still alive, while the submitted run has not been
    /// observed yet, or while the Backend's newest user message is a
    /// cola-authored Supplement newer than this turn's anchor with no assistant
    /// reply after it. `transcript` is the snapshot the caller just read. The
    /// Supplement is classified before the run state so the re-check can tell
    /// the racing Supplement apart from a session that is merely busy.
    async fn drain_state(
        &mut self,
        handles: &TurnHandles,
        transcript: &SessionTranscript,
        timeout_ms: u64,
    ) -> DrainRead {
        // `/stop` interrupted this session's run: no answer is coming, so the
        // drain must end promptly instead of waiting out its bound on a
        // Supplement the abort left unanswered (no rendering may continue once
        // the session is stopped). The marker is cleared by the next Turn's
        // `start`. The finalization is logged once per Turn: the drain and the
        // re-check that follows it both read the same sticky marker.
        if handles.waits.is_stopped(&self.session_id).await {
            if !self.stop_finalization_logged {
                self.stop_finalization_logged = true;
                tracing::info!("turn drain: session {} was stopped; finalizing", self.session_id);
            }
            return DrainRead {
                state: DrainState::Settled,
                live: false,
                settled_readable: false,
                contact: true,
            };
        }
        // Capture the turn's anchor from this snapshot if the incremental poll
        // never saw it — the supplement comparison is meaningless without it,
        // and a short turn's first poll only ever runs here. The anchor is one
        // fact: the message's identity together with its server time.
        let anchor = {
            let mut cards = handles.cards.cards.lock().await;
            match cards.get_mut(&self.session_id) {
                Some(card) => {
                    render::capture_turn_anchor(&mut card.acc, transcript);
                    card.acc.turn_anchor.clone()
                }
                None => None,
            }
        };
        // A Supplement the Backend has not answered yet. Its `msg_cola_` id is
        // authoritative (ADR-0026); the anchor is the server's own time for
        // this turn's user message (#190). Cola's clock never enters here.
        if let Some(anchor) = anchor.as_ref()
            && let Some(newest_user) = transcript.newest_user()
            && let Some(created) = newest_user.time.map(|time| time.created)
            && crate::opencode::parsing::is_cola_message_id(newest_user.id.as_str())
            && created > anchor.created_ms
        {
            // No assistant message after it: either its new Turn has not
            // started yet or it is being answered — keep rendering either
            // way. (`/stop` was handled above; an aborted run leaves nothing
            // for this check to wait for.)
            let answered = transcript.messages.iter().any(|message| {
                message.role == MessageRole::Assistant
                    && message
                        .time
                        .as_ref()
                        .map(|time| time.created >= created)
                        .unwrap_or(false)
            });
            if !answered {
                return DrainRead {
                    state: DrainState::Supplement,
                    live: false,
                    settled_readable: false,
                    contact: true,
                };
            }
        }
        // The submit only SCHEDULED the run (ADR-0056): before any sign of it
        // is observed, a non-busy status can just mean "not registered yet",
        // and settling on it would finalize the card Done before a token
        // arrived. The start signs are a Busy/Retry status, a terminal finish,
        // a recorded failure, or a step CREATED at/after the anchor — never a
        // completed straddling step from a prior turn (the transcript's
        // membership includes such a straddler, so only the created-at/after
        // filter keeps it from flipping this). These signs say the run STARTED;
        // they never say it finished — a Wake's content is a step created
        // at/after the anchor too, so only [`SessionTranscript::settle`] may
        // decide the ending (ADR-0059): idle, with no Wake's Execution left
        // unanswered and no live Background Task. The snapshot's failure is
        // kept as the finalization fallback. A REJECTED submit is the
        // exception: no run was scheduled, so the error is the outcome and one
        // idle read settles.
        if let Some(anchor) = &anchor {
            let turn = transcript.turn_for_user(anchor);
            let produced_a_step = turn.messages.iter().any(|message| {
                message
                    .time
                    .as_ref()
                    .is_some_and(|time| time.created >= anchor.created_ms)
            });
            if turn.error.is_some() || turn.complete || produced_a_step {
                self.drain_started = true;
            }
            // Only a read that actually carries the turn's user message is
            // authoritative for the failure: an anchorless read (the message
            // has not landed, or is already compacted away) leaves the last
            // observed failure in place.
            if transcript.anchor_of_user(&self.cola_message_id).is_some() {
                self.last_drain_error = turn.error.clone();
            }
        }
        let observed = self.drain_started || self.submit_failed;
        match crate::bridge::bounded_call(
            "turn drain session status",
            timeout_ms,
            handles
                .backend
                .session_status(&self.session_id, Some(&self.directory)),
        )
        .await
        {
            // Alive: parts keep coming. `is_live` is Busy or Retry — a Retry
            // schedules the next attempt, so the run is still cola's to watch.
            Some(Ok(Some(status))) if status.is_live() => {
                self.drain_started = true;
                DrainRead {
                    state: DrainState::Running,
                    live: true,
                    settled_readable: false,
                    contact: true,
                }
            }
            // A non-busy status: the settle decision owns the ending (ADR-0059,
            // ADR-0062) once the run was observed; before that the
            // confirmation window still bounds an unregistered submit. The
            // failed and timed-out paths pass [`IdleRead::Unreadable`]: no
            // evidence the session is not live, so it can never decide the
            // Unreceived ending — see `settle_or_yield`.
            Some(Ok(_)) => DrainRead {
                state: self.settle_or_yield(transcript, anchor.as_ref(), observed, IdleRead::Idle),
                live: false,
                settled_readable: true,
                contact: true,
            },
            Some(Err(e)) => {
                tracing::warn!("turn drain session status: {}", e);
                DrainRead {
                    state: self.settle_or_yield(transcript, anchor.as_ref(), observed, IdleRead::Unreadable),
                    live: false,
                    settled_readable: false,
                    contact: false,
                }
            }
            // The bounded status call timed out: same rule as a failed read.
            None => DrainRead {
                state: self.settle_or_yield(transcript, anchor.as_ref(), observed, IdleRead::Unreadable),
                live: false,
                settled_readable: false,
                contact: false,
            },
        }
    }

    /// The ending a non-busy (or unreadable) status read gives the Turn
    /// (ADR-0059, ADR-0062): the single settle decision decides it —
    /// `Waiting` yields the card, `Unreceived` ends it when the submitted
    /// message never landed at an idle session, `Running` keeps observing (the
    /// read carries a Wake whose Execution has not reached its boundary yet:
    /// the Wake's content must never declare the Turn complete),
    /// `Complete`/`Failed` settle it. Before the run was observed the
    /// [`IDLE_CONFIRMATIONS`] window still bounds an unregistered submit: the
    /// submit only schedules execution (ADR-0056), so the decision may not
    /// judge the Turn on an early idle read.
    ///
    /// `read` distinguishes a readable non-busy status from a failed or
    /// timed-out one. An unreadable status counts as idle for a run whose
    /// message landed — the pre-settle rule's own treatment — but it is no
    /// evidence the Session is not live, so it can never decide the Unreceived
    /// ending: an anchorless submit keeps observing instead (the drain bound's
    /// hand-off and the follow's lost-contact grace own a Backend cola cannot
    /// see).
    fn settle_or_yield(
        &mut self,
        transcript: &SessionTranscript,
        anchor: Option<&TurnAnchor>,
        observed: bool,
        read: IdleRead,
    ) -> DrainState {
        if anchor.is_none() && read == IdleRead::Unreadable {
            return DrainState::Running;
        }
        if !observed {
            self.unstarted_reads += 1;
            if self.unstarted_reads < IDLE_CONFIRMATIONS {
                return DrainState::Running;
            }
        }
        Self::drain_ending(transcript.settle(anchor))
    }

    /// The [`DrainState`] one settle decision gives the drain — the ONE
    /// mapping, through the disposition table, so the drain (`settle_or_yield`)
    /// and the finalization (`finish`) cannot read a decision differently.
    /// [`TurnSettle::Failed`] settles like the true end deliberately: the
    /// drain's control flow only needs "the ending is decided" — the failure's
    /// message is read from the same final transcript through the existing
    /// [`Turn::turn_error`] projection, so it is not re-plumbed here.
    fn drain_ending(settle: TurnSettle) -> DrainState {
        match Disposition::from(settle) {
            // The ending is not decided (a Wake's Execution has not reached
            // its boundary yet): keep observing.
            Disposition::Observe => DrainState::Running,
            Disposition::Waiting => DrainState::Waiting,
            Disposition::Unreceived => DrainState::Unreceived,
            // A decided ending settles the drain's read. The loop-only
            // dispositions (the sticky `/stop`, the two graces) are never
            // settle outcomes.
            Disposition::Done
            | Disposition::Failed(_)
            | Disposition::Stopped
            | Disposition::LostContact
            | Disposition::StuckPanel => DrainState::Settled,
        }
    }

    /// The failure a submitted turn recorded, read from the settled transcript:
    /// the newest assistant message's error inside the turn anchored at this
    /// Turn's `msg_cola_` user message (a recovered earlier step is not a
    /// failure). `None` when the read has no such anchor or the turn recorded
    /// none — a read hiccup never invents one.
    fn turn_error(&self, transcript: &SessionTranscript) -> Option<String> {
        let anchor = transcript.anchor_of_user(&self.cola_message_id)?;
        transcript.turn_for_user(&anchor).error
    }

    /// The drain's Backend read, bounded by the unified per-read timeout
    /// (`follow_read_timeout_ms`) so a hung Backend fails fast instead of
    /// freezing the tick; the lost-contact grace is the ceiling when reads
    /// never answer.
    async fn drain_transcript(&self, handles: &TurnHandles, timeout_ms: u64) -> Option<SessionTranscript> {
        match crate::bridge::bounded_call(
            "turn drain transcript",
            timeout_ms,
            handles.backend.transcript(&self.session_id),
        )
        .await
        {
            Some(Ok(transcript)) => Some(transcript),
            Some(Err(e)) => {
                tracing::warn!("turn drain transcript: {}", e);
                None
            }
            None => None,
        }
    }
}

/// Upper bound on continuation cards sent for one flush (each is a new
/// message): content beyond it is reconciled on the next flush. A pending
/// split gets one slot past the bound, so a Supplement is never refused by it.
pub(crate) const MAX_CARD_CHAIN: usize = 8;

/// The injectable timings of the out-of-turn settle loop the follow and the
/// Wake continuation share ([`settle`]): the render poll cadence, the
/// per-read bound, and the lost-contact / stuck-panel grace (also the
/// unreceived watch's waiting-hint delay, ADR-0062). Built by each caller
/// from its own injected atomics (the follow's `TurnConfig`, the external
/// flow's cadences), so the loop never reads a global.
#[derive(Clone, Copy)]
pub(crate) struct SettleTiming {
    pub(crate) poll_ms: u64,
    pub(crate) read_timeout_ms: u64,
    pub(crate) grace_ms: u64,
}

/// Why a Card Chain split was requested: the cause owns both its receipt line
/// and its handoff rule, so the flush never re-lists the causes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SplitKind {
    /// A Supplement landed below the live card (ADR-0043).
    Supplement,
    /// The user explicitly pulled the live card down with `/card`
    /// (ADR-0043, 2026-09-22 amendment).
    Pull,
    /// A Wake resumed the Turn after its card yielded 等待后台任务 or ended
    /// (ADR-0059): the chain continues on a new card below the user's
    /// message, and that new message is the notification.
    Wake,
    /// A Wake-less part the Backend wrote after the run already reported idle
    /// — the chain's card is past its wait (spec #602, ticket #606). It opens
    /// the same kind of continuation card a Wake split does, but with a
    /// NEUTRAL receipt: nothing resumed, so 「已恢复执行」 would lie. A
    /// defensive floor the same-snapshot ending rule (#604) makes
    /// near-unreachable, bounded to one card per request by the chain's
    /// `StreamAccumulator::residual_card_posted` marker.
    Residual,
}

/// The receipt line a Supplement's continuation carries (ADR-0043).
const SUPPLEMENT_RECEIPT: &str = "📨 已收到补充";

/// The `/card` pull's continuation line (ADR-0043, 2026-09-22 amendment).
const PULL_RECEIPT: &str = "⏬ 实时卡片已移到底部";

/// The 承接 line a Wake continuation opens with (ADR-0059): the new card is
/// itself the notification, and this line says why it appeared — the Turn was
/// resumed by the Backend after going idle. Source-neutral on purpose: a Wake
/// can be a finished background task, a subagent, an interruption or a server
/// restart, and cola never claims more than "the work resumed".
const WAKE_RECEIPT: &str = "🔔 已恢复执行，继续处理…";

/// The neutral receipt a Wake-less RESIDUAL continuation opens with (spec
/// #602, ticket #606): the Backend wrote a part after the run already reported
/// idle, so cola shows it honestly — the content is never dropped, and the
/// card never claims a resumption that did not happen. Deliberately neutral
/// ("there is more") rather than a claim. A defensive floor: the same-snapshot
/// ending rule (#604) makes this near-unreachable.
const RESIDUAL_RECEIPT: &str = "📄 还有更新";

/// The opening line a new Turn's card carries when cola owned no live chain
/// but the Backend's ADVISORY status read reported the Session live
/// (ADR-0062): the message rides the merge delivery (the V2 steer), so a
/// genuinely live run merges it exactly as a Supplement would — and the card
/// says so instead of promising a fresh run. Never a routing key itself: a
/// stale read only changes this line, never the route.
const MERGE_OPENING: &str = "📨 已收到，将并入当前运行";

/// The neutral line a card gains while the Session reads live but the Turn's
/// submitted message has not landed (ADR-0062), after the follow grace — a
/// genuine long tool call and a dead run look identical from outside, so cola
/// does not nag early. Not a claim, not an error: the wait may still end in
/// the run merging the message, or in the Unreceived ending.
const RECEIVE_HINT: &str = "⏳ 等待当前运行接收…";

impl SplitKind {
    /// The one receipt line this cause's continuation carries: every queued
    /// split gets exactly one, in arrival order.
    pub(crate) fn receipt(self) -> &'static str {
        match self {
            Self::Supplement => SUPPLEMENT_RECEIPT,
            Self::Pull => PULL_RECEIPT,
            Self::Wake => WAKE_RECEIPT,
            Self::Residual => RESIDUAL_RECEIPT,
        }
    }

    /// Whether this cause continues a card that has already ENDED: a Wake
    /// arrives after the Turn yielded 等待后台任务 or its card ended, and the
    /// neutral residual always fires on a card past its wait (terminal or
    /// restart-stamped), so the handoff must not restamp a terminal card's
    /// ending and the continuation starts a fresh live phase (the ended
    /// attempt's display facts stay behind). Every other cause splits a card
    /// that is still live.
    pub(crate) fn continues_an_ended_card(self) -> bool {
        matches!(self, Self::Wake | Self::Residual)
    }
}

/// Why a waiting card was collected (ADR-0059, spec #405): the reason owns the
/// collected card's header, so a collect site cannot restamp the wrong copy.
/// Part of the [`Turn`] ↔ handles interface: the Active-Session mapping
/// operations call [`Turn::collect_waiting`] with [`Self::SwitchedAway`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CollectReason {
    /// A new Turn in the thread superseded the wait.
    Superseded,
    /// The Session stopped being the thread's Active Session.
    SwitchedAway,
}

impl CollectReason {
    /// The state a collected waiting card records for this reason.
    fn collected(self) -> crate::feishu::card::CardState {
        match self {
            Self::Superseded => crate::feishu::card::CardState::Superseded,
            Self::SwitchedAway => crate::feishu::card::CardState::SwitchedAway,
        }
    }
}

/// What one Session Sync read did to a yielded (Waiting) card in place
/// (ADR-0060): the freeze's carve-out answers what the read owes — the ledger
/// refresh and the quiet true end ride the same read.
pub(crate) enum YieldedUpdate {
    /// The read owed nothing: no PATCH at all, the card stays exactly as the
    /// yield left it.
    Unchanged,
    /// The ledger moved (a row joined or left, a rendered second advanced, an
    /// entry arrived): the card is PATCHed in place and the wait goes on.
    Refreshed,
    /// The last Background Task retired and the read judged the true end: the
    /// card settled in place and stopped updating. `disposition` is the ending
    /// the read decided and the accumulator applied (#540), so the caller
    /// announces with the one table's classification and copy. `notice_at` is
    /// the Completion Notice's clock — `Some` when this card owes the notice
    /// (the Turn's own waiting card), `None` when it must stay silent (a Wake
    /// continuation, whose own send was the notification, or a card with no
    /// recorded turn start). `card_message_id` is the identity of the card the
    /// ending landed on (spec #602, review finding 6), so the caller's notice is
    /// bound to it and can never ride a newer Turn's card.
    Settled {
        disposition: Disposition,
        notice_at: Option<std::time::Instant>,
        card_message_id: Option<String>,
    },
    /// The refresh's write was permanently refused (review, PR #595): the card
    /// is suspended and nothing will ever carry this read's ledger delta or
    /// its retirement entries. The caller records nothing — the tasks stay
    /// live for a card that can render their entries — and claims neither
    /// `Refreshed` nor `Settled`.
    Refused,
}

impl YieldedUpdate {
    /// Whether this read's write was accepted — delivered, or owed by the
    /// delivery layer's retry (ADR-0067). The one gate every caller's record
    /// reads (review, PR #595): an `Unchanged` card admitted nothing and a
    /// `Refused` one will never write, so neither may record.
    pub(crate) fn accepted(&self) -> bool {
        matches!(self, Self::Refreshed | Self::Settled { .. })
    }
}

/// How a card that becomes a Session's live card disposes of the DIFFERENT
/// recorded predecessor it replaces (ADR-0063).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PredecessorCollect {
    /// Collect the predecessor now, as taken over — an external arm and a
    /// Wake continuation armed after a restart (the fresh Turn uses
    /// [`Turn::take_over_card_deferring_collect`] instead).
    Now,
    /// Hand the predecessor back **uncollected**: the fresh Turn's seed (spec
    /// #561, ticket #565) reads the orphan first, and only its result can tell
    /// the collect whether the orphan card's running `⏳` panels resolved onto
    /// the successor — so the collect happens after the seed. The record and
    /// the in-memory card id are already the successor's when this returns,
    /// so nothing else owns the orphan in between.
    Deferred,
    /// Collect nothing: the send continues the same chain (a split's
    /// continuation, a re-adopt), whose predecessor already ended.
    Never,
}

/// The Turn's card-delivery interface (spec #298, A2a): the operations sibling
/// flows invoke when they own the trigger moment — the render poll, the
/// request poller surfacing an inline block, the external renderer finalizing
/// a card, a Supplement arriving mid-turn, or an explicit `/card` pull. The
/// flush/split state machine behind them is private to the `flush` submodule.
impl Turn {
    /// Flush `session_id`'s live card to Feishu now, under the session's
    /// card-write lock.
    ///
    /// One card writer per session at a time. A flush is a read-send-record
    /// sequence, and callers are concurrent (the render poll, the request
    /// poller surfacing a block, a click's ack fallback); interleaved, the
    /// second writer still names the card the first just finalized and PATCHes
    /// its continuation slice onto it — two identical messages, only one
    /// tracked and repaintable. The resolution paths take the same lock
    /// (`resolve_blocks`), so a click cannot be overwritten by a stale flush.
    ///
    /// Returns what became of the writes this flush issued ([`FlushOutcome`],
    /// review, PR #595): the caller's record gate reads
    /// [`FlushOutcome::accepted`] — a permanently refused payload must not be
    /// recorded as delivered.
    pub(crate) async fn flush_card(cards: &CardsHandle, session_id: &str) -> FlushOutcome {
        let write_lock = cards.write_lock(session_id).await;
        let _guard = write_lock.lock().await;
        flush::flush_card_locked(cards, session_id, flush::SplitPolicy::Allow).await
    }

    /// Record that `card_message_id` is now `session_id`'s live card
    /// (ADR-0063): the durable fact Session Sync's reap needs after a restart.
    /// Every path that sends a card INTO the Session tracks it — a Turn's
    /// loading card, an external renderer's arm, a chain's continuation send —
    /// so the record always names the one card a restart should reap.
    ///
    /// `collect` is how a DIFFERENT previously recorded card is disposed of:
    /// [`PredecessorCollect::Now`] collects it as taken over where this send
    /// opens a new chain over an orphan (a Wake continuation armed after a
    /// restart, an external arm), [`PredecessorCollect::Deferred`] hands it
    /// back uncollected (the fresh Turn's takeover seed, spec #561, decides the
    /// collect only after its read), and [`PredecessorCollect::Never`] is for
    /// a send that continues the same chain (a split's continuation, a
    /// re-adopt), whose predecessor the split — or the static snapshot it
    /// replaces — already ended. The three takeover sends go through
    /// [`Self::take_over_card`] (or its deferred sibling), which owns the
    /// attach-then-collect order. Returns the predecessor record this send
    /// took over — `Some` exactly when a DIFFERENT recorded card was handed
    /// over — so the caller can read the orphan's anchor and cursor (the
    /// fresh-Turn seed, ticket #565).
    ///
    /// `directory` is the Session's directory when the caller knows it (a
    /// Turn's mapping, an external arm); `None` falls back to the card's own
    /// work context. The reap's reads route by it, so an unmapped restart can
    /// still ask the right instance (ADR-0063, #438).
    pub(crate) async fn track_live_card(
        cards: &CardsHandle,
        session_id: &str,
        card_message_id: &str,
        collect: PredecessorCollect,
        directory: Option<&str>,
    ) -> Option<ChainRecord> {
        let (message_id, created_ms, context_directory, reply_to) = {
            let live = cards.cards.lock().await;
            match live.get(session_id) {
                Some(card) => (
                    card.acc.cola_message_id().map(MessageId::new).or_else(|| {
                        card.acc
                            .turn_anchor
                            .as_ref()
                            .map(|anchor| anchor.message_id.clone())
                    }),
                    card.acc.turn_anchor.as_ref().map(|anchor| anchor.created_ms),
                    card.acc.directory().map(str::to_string),
                    // The chain's durable reply target (issue #580): the Feishu
                    // message the card replies under — an OpenCode message id
                    // is never a deliverable target.
                    card.acc.reply_to_message_id().map(str::to_string),
                ),
                None => (None, None, None, None),
            }
        };
        // A card with no Turn message to scope a settle decision with cannot be
        // reaped: no record, no reap attempt after a restart.
        let message_id = message_id?;
        let directory = directory
            .map(str::to_string)
            .filter(|directory| !directory.is_empty())
            .or(context_directory);
        // A TAKEOVER (a different recorded predecessor is being replaced)
        // records the owed orphan gap in the same write that re-points the
        // record (spec #561, review #569): a crash before the takeover's read
        // returns must not lose the orphaned tail. A continuation tracks
        // plainly — its chain is the same one, still rendering.
        let previous = match collect {
            PredecessorCollect::Never => cards.chains.track(
                session_id,
                card_message_id,
                message_id,
                created_ms,
                directory.as_deref(),
                reply_to.as_deref(),
            ),
            _ => cards.chains.track_takeover(
                session_id,
                card_message_id,
                message_id,
                created_ms,
                directory.as_deref(),
                reply_to.as_deref(),
            ),
        };
        // A re-point within the chain carries the Rendered Cursor (spec #561).
        // Seed a fresh accumulator's empty base with the carried fact — a new
        // Turn's or a takeover's card must not clear the chain's frontier —
        // while a split continuation's accumulator already carries the same
        // value and keeps its own.
        if let Some(cursor) = previous.as_ref().and_then(|record| record.cursor.clone()) {
            let mut live = cards.cards.lock().await;
            if let Some(card) = live.get_mut(session_id) {
                card.acc.seed_cursor_if_empty(cursor);
            }
        }
        if collect == PredecessorCollect::Never {
            return None;
        }
        let previous = previous?;
        if previous.card_message_id == card_message_id {
            return None;
        }
        if collect == PredecessorCollect::Now {
            crate::bridge::chain::collect_orphan(cards, session_id, &previous.card_message_id).await;
        }
        Some(previous)
    }

    /// The split continuation's atomic transition (spec #561, review #569):
    /// re-point the record onto the new card AND settle the create's confirmed
    /// Rendered Cursor in ONE chains write — the shape the projection's
    /// takeover uses. Two writes leave a window where the record names the
    /// continuation with the predecessor's frontier; a crash right after
    /// Feishu accepted the create would let the next recovery project content
    /// the continuation already shows. `expected` is the exact stage the
    /// create carried: a superseded stage re-points with the carried cursor
    /// alone, exactly as [`Self::track_live_card`] does. A create's stage
    /// names no card (its id is unknown until it lands).
    pub(crate) async fn track_continuation_card(
        cards: &CardsHandle,
        session_id: &str,
        card_message_id: &str,
        expected: state::StagedCursorId,
    ) {
        let (message_id, created_ms, context_directory, reply_to, confirmed) = {
            let mut live = cards.cards.lock().await;
            let Some(card) = live.get_mut(session_id) else {
                return;
            };
            let confirmed = card.acc.take_staged_cursor(card_message_id, expected);
            (
                card.acc.cola_message_id().map(MessageId::new).or_else(|| {
                    card.acc
                        .turn_anchor
                        .as_ref()
                        .map(|anchor| anchor.message_id.clone())
                }),
                card.acc.turn_anchor.as_ref().map(|anchor| anchor.created_ms),
                card.acc.directory().map(str::to_string),
                // The chain's durable reply target (issue #580): a split whose
                // supplement moved the anchor carries the newest one.
                card.acc.reply_to_message_id().map(str::to_string),
                (confirmed),
            )
        };
        // A card with no Turn message to scope a settle decision with cannot be
        // reaped: no record, no reap attempt after a restart — the same rule
        // [`Self::track_live_card`] applies.
        let Some(message_id) = message_id else {
            return;
        };
        let previous = match confirmed {
            Some((cursor, gap)) => cards.chains.track_carrying_cursor(
                session_id,
                card_message_id,
                message_id,
                created_ms,
                context_directory.as_deref(),
                reply_to.as_deref(),
                &cursor,
                gap.as_ref()
                    .map(|coverage| (&coverage.frontier, coverage.complete)),
            ),
            None => cards.chains.track(
                session_id,
                card_message_id,
                message_id,
                created_ms,
                context_directory.as_deref(),
                reply_to.as_deref(),
            ),
        };
        // A re-point within the chain carries the Rendered Cursor (spec #561):
        // seed a fresh accumulator's empty base with the carried fact, while a
        // split continuation's accumulator already carries the same value (the
        // stage's take advanced it) and keeps its own.
        if let Some(cursor) = previous.as_ref().and_then(|record| record.cursor.clone()) {
            let mut live = cards.cards.lock().await;
            if let Some(card) = live.get_mut(session_id) {
                card.acc.seed_cursor_if_empty(cursor);
            }
        }
    }

    /// Make `card_message_id` the session's live card, taking the chain over
    /// from whatever card the durable record still names (ADR-0063): attach the
    /// id to the in-memory card FIRST — so a reap tick racing the send never
    /// reads the record naming a card the in-memory chain has not admitted yet
    /// — then track it with a DIFFERENT recorded predecessor collected as taken
    /// over. A send that opens a new chain over an orphan (a fresh Turn's
    /// loading card, an external arm, a Wake continuation after a restart) takes
    /// over; a send that continues the same chain (a split's continuation, a
    /// re-adopt) tracks with [`PredecessorCollect::Never`] instead.
    ///
    /// `directory` is the Session's directory when the caller knows it; `None`
    /// falls back to the card's own work context (see [`Self::track_live_card`]).
    /// Returns the predecessor record this takeover collected, when it named a
    /// different card (see [`Self::track_live_card`]).
    pub(crate) async fn take_over_card(
        cards: &CardsHandle,
        session_id: &str,
        card_message_id: &str,
        directory: Option<&str>,
    ) -> Option<ChainRecord> {
        Self::attach_card_message_id(cards, session_id, card_message_id).await;
        Self::track_live_card(
            cards,
            session_id,
            card_message_id,
            PredecessorCollect::Now,
            directory,
        )
        .await
    }

    /// [`Self::take_over_card`] with the predecessor's collect **deferred** to
    /// the caller (spec #561, ticket #565): the fresh Turn takes the chain over
    /// first, then resolves the takeover seed, and only the seed's result can
    /// tell the collect whether the orphan card's running `⏳` panels resolved
    /// onto the successor — so the seed reads the orphan before the collect
    /// strips anything. The record and the in-memory card id are already the
    /// successor's when this returns, so nothing else can own the orphan in
    /// between. Returns the predecessor to seed from — `None` when no different
    /// card was taken over — and the caller must collect it with
    /// [`crate::bridge::chain::collect_orphan_after_takeover`] once the seed
    /// has run.
    pub(crate) async fn take_over_card_deferring_collect(
        cards: &CardsHandle,
        session_id: &str,
        card_message_id: &str,
        directory: Option<&str>,
    ) -> Option<ChainRecord> {
        Self::attach_card_message_id(cards, session_id, card_message_id).await;
        Self::track_live_card(
            cards,
            session_id,
            card_message_id,
            PredecessorCollect::Deferred,
            directory,
        )
        .await
    }

    /// Attach `card_message_id` to the session's in-memory card, when it still
    /// has one (a card replaced in the released moment is never touched).
    async fn attach_card_message_id(cards: &CardsHandle, session_id: &str, card_message_id: &str) {
        if let Some(card) = cards.cards.lock().await.get_mut(session_id) {
            card.card_message_id = Some(card_message_id.to_string());
        }
    }

    /// Seed the fresh Turn's card from the orphaned chain's Rendered Cursor
    /// (spec #561, ticket #565): one bounded Session Transcript read, resolved
    /// into the projection seed the reap's adoption also uses — the orphaned
    /// Turn's window cut at the confirmed frontier, plus its live set resolved
    /// by identity — so a message that wins the race against the adoption
    /// still shows the run's final undelivered tail exactly once, as a
    /// continuation before the new Turn's content.
    ///
    /// The seed lands only while `successor_card_id` is still the session's
    /// card: the read runs unlocked, and a card another Turn put there
    /// meanwhile must never receive the orphan's content — it belongs to no
    /// successor of it. A record with no anchor seeds nothing. A CURSORLESS
    /// record keeps today's carry as the seed's live-set fallback: the
    /// orphaned Turn's still-live calls resolve by identity, no content
    /// replays. A CURSOR-BEARING record's gap is never dropped (spec #561,
    /// review #569): when the read fails, times out, or cannot place the
    /// cursor, the seed stays PENDING on the accumulator — the chain's cursor
    /// pinned at the gap's frontier — and a later render read lands the
    /// undelivered tail ([`render`]'s pending-seed retry). One INFO line
    /// records the decision (session + outcome, never chat content). Returns
    /// the live-set calls the seed RESOLVED onto the successor — empty when
    /// nothing landed — which the takeover's collect strips from the orphan
    /// card's running `⏳` panels (spec #561, review #569).
    ///
    /// Only a fresh Turn's takeover calls this: a Wake continuation keeps
    /// ADR-0061's no-replay scope and the reap/external arms never seed.
    async fn seed_orphan_delta(
        handles: &TurnHandles,
        session_id: &str,
        successor_card_id: &str,
        orphan: &ChainRecord,
    ) -> Vec<String> {
        let Some(anchor) = orphan.anchor() else {
            tracing::info!("restart seed: session {session_id} none (no anchor)");
            return Vec::new();
        };
        let read = crate::bridge::bounded_call(
            "restart seed transcript",
            handles.config.follow_read_timeout_ms(),
            handles.backend.transcript(session_id),
        )
        .await;
        let transcript = match read {
            Some(Ok(transcript)) => transcript,
            Some(Err(error)) => {
                tracing::debug!("restart seed: session {session_id} read failed: {error}");
                tracing::info!("restart seed: session {session_id} none (read failed)");
                // A cursor-bearing orphan's tail must not be lost: record it
                // as the durable gap (spec #561, review #569).
                Self::note_pending_gap(handles, session_id, successor_card_id, orphan).await;
                return Vec::new();
            }
            None => {
                tracing::info!("restart seed: session {session_id} none (read timed out)");
                Self::note_pending_gap(handles, session_id, successor_card_id, orphan).await;
                return Vec::new();
            }
        };
        // The gap a takeover from a cursor OWES durably (spec #561, review
        // #569), whether or not this read can place the cursor: the old card
        // is collected here, so a crash before the new Turn's first confirmed
        // write would leave the record with the old cursor and the new Turn's
        // anchor — a recovery that cannot see the orphaned Turn's window and
        // would omit its undelivered tail. `bound` is the Turn the chain moved
        // to (the successor card's own Turn message, exactly as the takeover
        // persisted it).
        let bound = Self::current_turn_message(handles, session_id).await;
        // A TRUNCATED read is a prefix (spec #561, review #569): the cursor
        // must NOT resolve against it — the visible prefix may render (the gap
        // walk does that), but the gap's end may be beyond the page cap, so
        // only the live-set fallback applies and nothing may mark the gap
        // complete until a complete read shows its end.
        let seed = match &orphan.cursor {
            Some(cursor) if !transcript.truncated => {
                match state::CursorSeed::for_orphan_resolving(&transcript, cursor, &anchor) {
                    Some(seed) => seed,
                    None => state::CursorSeed::live_calls_only(&transcript, &anchor),
                }
            }
            Some(_) => state::CursorSeed::live_calls_only(&transcript, &anchor),
            None => state::CursorSeed::live_calls_only(&transcript, &anchor),
        };
        let resolved = seed.resolved_calls();
        // The cursor the seed derives from: the orphan's confirmed one, or the
        // default a cursorless record starts from.
        let cursor = orphan.cursor.clone().unwrap_or_default();
        let applied = {
            let mut live = handles.cards.cards.lock().await;
            let landed = Self::apply_orphan_seed(&mut live, session_id, successor_card_id, &cursor, seed);
            // A gap the takeover already carried is the more accurate fact
            // (the record's own cursor may have moved past it): the carry
            // already noted it durably and seeded the accumulator.
            if landed
                && let Some(gap_cursor) = orphan.cursor.clone()
                && orphan.pending_gap.is_none()
                && let Some(card) = live.get_mut(session_id)
            {
                // The same session/card check `apply_orphan_seed` just made.
                let gap = crate::bridge::chain::PendingGap {
                    cursor: gap_cursor,
                    anchor: anchor.clone(),
                    bound: bound.clone(),
                };
                handles
                    .cards
                    .chains
                    .note_pending_gap(session_id, successor_card_id, &gap);
                // The resolved seed's scope walk renders the tail in-process
                // (tagging it as gap content and marking `gap_rendered`), so
                // the durable fact — cleared only by a confirmed write that
                // covers the gap's end — never double-renders it here.
                card.acc.set_pending_gap(gap);
            }
            landed
        };
        tracing::info!(
            "restart seed: session {session_id} resolved {} live calls (applied {applied})",
            resolved.len()
        );
        if applied { resolved } else { Vec::new() }
    }

    /// The Turn message the chain has moved to (spec #561, review #569): the
    /// live card's own cola message, which bounds an orphan gap's window. `None`
    /// when the card cannot name one — the gap then falls back to the read.
    async fn current_turn_message(handles: &TurnHandles, session_id: &str) -> Option<MessageId> {
        let live = handles.cards.cards.lock().await;
        live.get(session_id)
            .and_then(|card| card.acc.cola_message_id().map(str::to_string))
            .map(MessageId::new)
    }

    /// Queue a cursor-bearing orphan's undelivered tail as the durable gap on
    /// its successor's record (spec #561, review #569) when the takeover's read
    /// could not even supply a transcript: the gap is noted and the successor's
    /// accumulator renders it once a read places it. A cursorless orphan has no
    /// frontier to lose and keeps today's behavior, and a gap the takeover
    /// already carried stays as it is (its cursor may have advanced past).
    async fn note_pending_gap(
        handles: &TurnHandles,
        session_id: &str,
        successor_card_id: &str,
        orphan: &ChainRecord,
    ) {
        if orphan.pending_gap.is_some() {
            return;
        }
        let Some(cursor) = orphan.cursor.clone() else {
            return;
        };
        let Some(anchor) = orphan.anchor() else {
            return;
        };
        let bound = Self::current_turn_message(handles, session_id).await;
        let gap = crate::bridge::chain::PendingGap {
            cursor,
            anchor,
            bound,
        };
        handles
            .cards
            .chains
            .note_pending_gap(session_id, successor_card_id, &gap);
        let mut live = handles.cards.cards.lock().await;
        if let Some(card) = live.get_mut(session_id)
            && card.card_message_id.as_deref() == Some(successor_card_id)
        {
            card.acc.set_pending_gap(gap);
        }
    }

    /// Apply a resolved seed to `session_id`'s accumulator, but only while that
    /// card session is still `successor_card_id`'s: the caller's transcript
    /// read runs unlocked, and a card another Turn replaced meanwhile must
    /// never receive the orphan's content — it belongs to no successor of it.
    /// Returns whether the seed landed.
    fn apply_orphan_seed(
        live: &mut std::collections::HashMap<String, state::CardSession>,
        session_id: &str,
        successor_card_id: &str,
        cursor: &RenderedCursor,
        seed: state::CursorSeed,
    ) -> bool {
        match live.get_mut(session_id) {
            Some(card) if card.card_message_id.as_deref() == Some(successor_card_id) => {
                card.acc.seed_projection(cursor, seed);
                true
            }
            // The successor was replaced during the read: nothing to seed.
            _ => false,
        }
    }

    /// Split `session_id`'s Card Chain at a user message (ADR-0043): append the
    /// split to the chain's split queue and flush. The flush finalizes the live
    /// card with the standard split header (keeping everything before the split)
    /// and sends a continuation that replies to the NEWEST queued split, carrying
    /// one receipt per queued split plus only the content that arrives after the
    /// split — so the live card stays the newest message and no supplement is
    /// coalesced away. `kind` selects the receipt line and nothing else: a
    /// Supplement and an explicit `/card` pull follow the same finalize-and-handoff
    /// path. Runs under the session's card-write lock, so concurrent requests
    /// queue in arrival order and the finalization reuses the size-split path (the
    /// live Interaction Blocks migrate to the continuation; the previous card's
    /// controls are settled). No-op when the session has no live card.
    pub(crate) async fn split_card_chain(
        cards: &CardsHandle,
        session_id: &str,
        reply_to: &str,
        kind: SplitKind,
    ) {
        let write_lock = cards.write_lock(session_id).await;
        let _guard = write_lock.lock().await;
        {
            let mut live = cards.cards.lock().await;
            let Some(card) = live.get_mut(session_id) else {
                return;
            };
            card.pending_split.push(state::PendingSplit {
                reply_to: reply_to.to_string(),
                kind,
                receipt_pushed: false,
                // A Supplement's or a pull's content always arrives after the
                // split: the receipt keeps cola's "now" key.
                line: None,
                // Only a Wake continuation's handover writes the ledger's
                // outgoing facts (ADR-0060).
                handover: false,
            });
        }
        flush::flush_card_locked(cards, session_id, flush::SplitPolicy::Allow).await;
    }

    /// Push the neutral waiting line ([`RECEIVE_HINT`]) onto `session_id`'s
    /// card, exactly once (ADR-0062): while the Session reads live but the
    /// Turn's submitted message has not landed, the card gains the line once
    /// the follow grace passes — a genuine long tool call and a dead run look
    /// identical from outside, so cola does not nag early. Returns whether the
    /// line was pushed now (the caller flushes it); false when the line is
    /// already shown or the card is gone.
    pub(crate) async fn show_receive_hint(cards: &CardsHandle, session_id: &str) -> bool {
        let mut live = cards.cards.lock().await;
        let Some(card) = live.get_mut(session_id) else {
            return false;
        };
        if card.acc.receive_hint_shown() {
            return false;
        }
        card.acc.mark_receive_hint_shown();
        card.acc.push_receipt(RECEIVE_HINT);
        true
    }

    /// Hand an ENDED card chain over to a Wake continuation or the neutral
    /// residual floor (ADR-0059, spec #602/#606): write the ledger handover the
    /// continuation owes the chain it is leaving (ADR-0060), queue the
    /// continuation split and flush it, so the tracked card is finalized and a
    /// NEW continuation card — replied to `reply_to`, opening with the cause's
    /// receipt — becomes the chain's newest card. A Waiting card is stamped
    /// 「部分完成，继续中…」 (its wait is over, the chain moves on); a terminal
    /// card keeps the ending it recorded — unless the handover just wrote it, in
    /// which case even a terminal card is PATCHed, with its ending intact.
    /// Unlike [`Self::split_card_chain`], the chain must NOT be owned by a live
    /// Turn/renderer: the Wake decision reads a snapshot, and a continuation
    /// must never split a card somebody else is still streaming into. `line`
    /// carries the receipt's key (just before the resumed/legacy work, whose
    /// server times are already in the past at poll time) and — for a Wake —
    /// the Wake it covers; a `line.wake` of `None` is the neutral residual,
    /// which no Wake announced and which therefore opens the neutral receipt
    /// (never 「已恢复执行」).
    ///
    /// The handover is part of the SAME write-lock-held sequence as the enqueue
    /// and the flush ([`CardsHandle::write_lock`]): inside the cards lock,
    /// after the render-owned guard has admitted the split, the read's
    /// remaining live list and the retiring Wakes' fixed completion entries are
    /// written onto the outgoing accumulator — so the handover PATCH carries
    /// both and the continuation, whose slice starts after them, renders only
    /// its 承接 line and the remaining list. A refused split writes nothing,
    /// and a card with no Turn anchor to scope the entries with owes no
    /// handover (`handover = false`) while the split still proceeds. `now_ms`
    /// is the read's clock — the Session Sync pass's own, shared with the
    /// yielded refresh of the same read.
    ///
    /// Returns `None` — nothing queued — when the session has no card or a live
    /// renderer owns it. `Some(carried)` once the split is queued and flushed,
    /// where `carried` says whether the outgoing accumulator's ledger write ran
    /// AND the flush that carried it was accepted — delivered or owed by the
    /// delivery layer's retry (review, PR #595; [`FlushOutcome::accepted`]): the
    /// read's remaining live list and its retirement entries rode the finalize
    /// PATCH (the caller's carrier signal for its reconcile pass). A card with
    /// no Turn anchor to scope the entries with still splits, but carries
    /// nothing (`Some(false)`), and so does a permanently refused write.
    pub(crate) async fn split_chain_for_continuation(
        cards: &CardsHandle,
        backend: &Arc<dyn crate::backend::Backend>,
        session_id: &str,
        reply_to: &str,
        line: ContinuationLine,
        transcript: &SessionTranscript,
        now_ms: i64,
    ) -> Option<bool> {
        // A line that names a Wake is the Wake continuation; one that names
        // none is the neutral residual (spec #602, #606). The cause selects the
        // receipt copy and, with it, whether the continuation claims a
        // resumption at all.
        let kind = if line.wake.is_some() {
            SplitKind::Wake
        } else {
            SplitKind::Residual
        };
        let write_lock = cards.write_lock(session_id).await;
        let _guard = write_lock.lock().await;
        // Plan the handover's completion entries under a brief read of the
        // card, then spend their one output read each OUTSIDE the cards lock
        // (spec #593; the reads are network). The write-lock stays held
        // throughout — this sequence's own serialization — while the cards
        // lock is released for the reads.
        let mut plans = {
            let live = cards.cards.lock().await;
            match live.get(session_id) {
                Some(card) if !card.acc.card_state().is_render_owned() => {
                    render::plan_ledger_entries(&card.acc, transcript, card.acc.turn_anchor.as_ref())
                }
                _ => return None,
            }
        };
        render::read_planned_outputs(backend, &mut plans).await;
        let handover = {
            let mut live = cards.cards.lock().await;
            let card = live.get_mut(session_id)?;
            if card.acc.card_state().is_render_owned() {
                return None;
            }
            // A card with no Turn anchor to scope the entries with owes no
            // handover: its outgoing read writes nothing, exactly as before
            // (spec #593 keeps the same gate — only the reads moved out).
            let handover = card.acc.turn_anchor.is_some()
                && render::apply_ledger_read(
                    &mut card.acc,
                    transcript,
                    &HashMap::new(),
                    now_ms,
                    state::LedgerCadence::Minute,
                    plans,
                );
            card.pending_split.push(state::PendingSplit {
                reply_to: reply_to.to_string(),
                kind,
                receipt_pushed: false,
                line: Some(line),
                handover,
            });
            handover
        };
        let flush = flush::flush_card_locked(cards, session_id, flush::SplitPolicy::Allow).await;
        // The carrier answer is the write's (review, PR #595): a handover the
        // flush could not accept (a permanent refusal) never reached the
        // outgoing card, so the caller's pass must not record.
        Some(handover && flush.accepted())
    }

    /// Resume a yielded card IN PLACE for a genuine resumption Wake
    /// (ADR-0066, spec #602): the one-card-per-request handoff, beside the
    /// split. One write-lock-held sequence, exactly like the split handover:
    /// the read's remaining live list and the retiring Wake's fixed completion
    /// entry are written onto the card's OWN accumulator, so the entry lands at
    /// its own moment on the card that hosted the task (ADR-0060) and the live
    /// list stays; the card then takes [`CardState::Resuming`] and the flush
    /// renders the whole live slice on the SAME card. No card is sent and
    /// nothing is replied to: the PATCH is the announcement, so it drains the
    /// staged Wake Watermark exactly as the 承接 line's send did (ADR-0061) —
    /// a restart after it cannot re-post the Wake.
    ///
    /// This is also the in-place render of a Wake-less tail and of a resumption
    /// card's further work (spec #602): the same resume, so no second
    /// continuation card ever opens on an already-resumed chain. `wake_id` is
    /// the completion this resume takes over, marked as such in the same locked
    /// write so the resume is never re-decided; `None` for a Wake-less tail,
    /// which has no completion to announce or hand over.
    ///
    /// The flush keeps [`SplitPolicy::Allow`]: a resumed run that outgrows one
    /// card still finalizes it and continues the chain on a new one (ADR-0066
    /// keeps the size split). The resumed run then reaches its ending through
    /// the shared out-of-turn settle loop, which the caller spawns — back to
    /// 「⏳ 等待后台任务」 while Background Tasks remain live, or the true end
    /// (✅/❌/⏹).
    ///
    /// Returns false — nothing written — when the card is no longer the
    /// yielded one this handoff admits
    /// ([`CardOwnership::admits_ledger_refresh`]: still `Waiting`, live, no
    /// split owed), so a race with a collect, a new Turn or a handoff can
    /// never resume a card somebody else took over — and when the resume's
    /// flush was permanently refused (review, PR #595), because the caller's
    /// record gate reads this same answer. `now_ms` is the read's clock — the
    /// Session Sync pass's own, shared with the split handover of the same read.
    pub(crate) async fn resume_yielded_card(
        cards: &CardsHandle,
        backend: &Arc<dyn crate::backend::Backend>,
        session_id: &str,
        wake_id: Option<&str>,
        transcript: &SessionTranscript,
        now_ms: i64,
    ) -> bool {
        let write_lock = cards.write_lock(session_id).await;
        let _guard = write_lock.lock().await;
        // Plan the entries this read would place under a brief read of the
        // card, then spend their one output read each OUTSIDE the cards lock
        // (spec #593; the reads are network).
        let mut plans = {
            let live = cards.cards.lock().await;
            match live.get(session_id) {
                Some(card) => {
                    render::plan_ledger_entries(&card.acc, transcript, card.acc.turn_anchor.as_ref())
                }
                None => Vec::new(),
            }
        };
        render::read_planned_outputs(backend, &mut plans).await;
        {
            // The yielded-card write admission, re-checked under the write lock
            // through the module's lock-scoped helper: the returned card is
            // still locked, so this admission and the write below share one
            // lock and no collect, split or ending can slip between them.
            let Some(mut card) = CardOwnership::admit_ledger_write(cards, session_id).await else {
                return false;
            };
            render::apply_ledger_read(
                &mut card.acc,
                transcript,
                &HashMap::new(),
                now_ms,
                state::LedgerCadence::Second,
                plans,
            );
            // The chain has taken this completion's work over, when there is
            // one. Its entry may already be on the card (a yielded ledger
            // refresh placed it while the work was unrendered): the
            // announcement set stays untouched, so the entry is never doubled
            // — only the handoff is recorded. A Wake-less tail marks nothing.
            if let Some(wake_id) = wake_id {
                card.acc.hand_over_wake(wake_id);
            }
            card.acc.set_resuming();
        }
        // The carrier answer is the write's (review, PR #595): a permanently
        // refused resume never reached the card, so the caller records nothing
        // and does not treat the chain as resumed.
        flush::flush_card_locked(cards, session_id, flush::SplitPolicy::Allow)
            .await
            .accepted()
    }

    /// Land a later Wake-less tail on the chain's ONE already-posted neutral
    /// residual card (spec #602, ticket #606): the "content is never dropped"
    /// half of the floor, after [`Self::wake_continuation`]'s
    /// [`WakeContinuation::Residual`] posted the one card. The tail renders on
    /// that same card in place — no second card, no resumption — and gains the
    /// neutral 「📄 还有更新」 receipt exactly once (the first in-place residual;
    /// a later tail keeps the receipt already carried). The ending the card
    /// already carries is restored over the render and
    /// carried by the single flush, so a settled (terminal) card's recorded
    /// ending is never rewritten (ADR-0074). Returns whether a write was
    /// accepted (the caller's carrier signal); the content itself stays owed
    /// through the delivery layer when the PATCH has to retry.
    ///
    /// One write-lock-held sequence, like the in-place resume: the admission
    /// (the chain's card, with no live renderer owning it) is re-checked under
    /// the write lock, the card renders the read WITHOUT its own flush (the
    /// ending restore must ride the same write), and the flush that carries
    /// the tail is the caller's carrier. The admission is broader than "the
    /// residual card": when the residual has no reachable reply target to
    /// split onto, the FIRST Wake-less tail on an existing card also lands here
    /// in place — the "content is never dropped" floor (#606) — and the write
    /// marks the floor fired, so later tails route to this same path.
    ///
    /// [`SplitPolicy::Allow`]: a tail that pushes the neutral card past
    /// Feishu's size limit is finalized and its remainder carried onto a new
    /// Card (the one allowed mid-request continuation besides a genuine
    /// resumption), so no content is ever built unsplit and lost to a refused
    /// oversized PATCH. The split stays NEUTRAL: no 「🔔 已恢复执行」 receipt and
    /// no Wake 承接 line (the residual has no Wake, so no `pending_split`
    /// receipt is queued) — "at most one residual Card per request" bounds the
    /// neutral RECEIPT card, not a size-split continuation.
    pub(crate) async fn render_residual_in_place(
        cards: &CardsHandle,
        sessions: &SessionsHandle,
        backend: &Arc<dyn crate::backend::Backend>,
        requests: &RequestsHandle,
        session_id: &str,
        transcript: &SessionTranscript,
    ) -> bool {
        let write_lock = cards.write_lock(session_id).await;
        let _guard = write_lock.lock().await;
        // The admission, re-checked under the write lock: the chain's
        // non-render-owned card admits. That is the chain's residual card (its
        // ONE neutral card posted, a later tail), and — when the residual has
        // no reachable reply target to split onto — the first Wake-less tail on
        // an existing card, which must never be dropped (#606). A card a live
        // renderer owns, or one a new Turn replaced, is left alone.
        let Some((ending_state, ending_error)) = ({
            let live = cards.cards.lock().await;
            live.get(session_id)
                .filter(|card| !card.acc.card_state().is_render_owned())
                .map(|card| {
                    (
                        card.acc.card_state().clone(),
                        card.acc.error().map(str::to_string),
                    )
                })
        }) else {
            return false;
        };
        // Render the read into the card WITHOUT its own flush: rendering the
        // new parts sets a live state, and the ending restore below must ride
        // the same write as those parts, so the card never PATCHes a live
        // header for content a settled run already finished.
        if render::render_and_flush_settling(cards, sessions, backend, requests, session_id, transcript)
            .await
            .is_none()
        {
            return false;
        }
        // Restore the ending the card carried before the late tail: rendering
        // the new parts set a live state, but a card past its wait keeps the
        // ending it recorded (ADR-0074). The one flush below carries both. The
        // floor is marked fired in the same write, so a LATER Wake-less tail
        // routes to this in-place path (`RenderResidualInPlace`) instead of
        // re-deciding `Residual`; when the residual instead split onto its own
        // neutral card, `push_queued_receipts` set the same marker at the send.
        {
            let mut live = cards.cards.lock().await;
            let Some(card) = live.get_mut(session_id) else {
                return false;
            };
            // The FIRST in-place residual gains the neutral residual receipt
            // (spec #602, ticket #606): the tail renders honestly on a
            // neutrally-labeled card, never a 「已恢复执行」 resumption, and the
            // receipt is written exactly once — this same locked write marks the
            // floor fired, so a later tail adds no second receipt. A card whose
            // one neutral residual already posted (the split arm set the marker)
            // keeps the receipt it already carries.
            if !card.acc.residual_card_posted() {
                card.acc.push_receipt(SplitKind::Residual.receipt());
            }
            card.acc.restore_ending(ending_state, ending_error);
            card.acc.mark_residual_card_posted();
            card.acc.refresh_phase();
        }
        // `SplitPolicy::Allow`: an over-budget tail finalizes this card and
        // continues on a new one — the size split, not a resumption. With no
        // queued split, no receipt is pushed, so the continuation stays neutral.
        flush::flush_card_locked(cards, session_id, flush::SplitPolicy::Allow)
            .await
            .accepted()
    }

    /// Refresh a yielded card's ledger from a Session Sync read, in place
    /// (ADR-0060): the freeze's carve-out, beside the Wake handover. A Waiting
    /// card has no render loop — its Turn yielded to its live Background Tasks
    /// — so the existing reads are what keep its section true: a task that
    /// retired quietly (its Wake's resumed run renders nothing, so no
    /// continuation is owed) leaves the live list while its fixed completion
    /// entry arrives on the host card, an elapsed row moves the moment its
    /// rendered second advances (the read's own cadence, far below Feishu's
    /// per-message cap), and a read that would render the ledger the card
    /// already shows leaves the card otherwise alone — no PATCH at all, so its
    /// header, timeline and footer stay frozen. The one exception is the read
    /// below: a read whose ledger did not change can still be the true end, and
    /// that read PATCHes the card as it settles.
    ///
    /// The same read is also the true end's judge (ADR-0060): when it shows
    /// the last Background Task retired, the card settles in place — ✅ when
    /// the read's own settle decision is complete (ADR-0059), ❌ with the
    /// failure's message when a settled failure dominates it, ⏹ 已停止 for a
    /// deliberate `/stop` — and reports the notice the caller owes
    /// ([`YieldedUpdate::Settled`]). A card that already ended (a settled
    /// failure or stop, or a collected wait) keeps the ending it recorded:
    /// only a `Waiting` card is ever stamped here. `stopped` is the session's
    /// sticky `/stop` marker, read by the caller; the stop dominates only once
    /// the read can judge the end too — a read still reporting live Background
    /// Tasks leaves the card waiting (its ledger still has rows to show).
    ///
    /// Only the yielded `Waiting` card is refreshed: a live card is
    /// render-owned (its own loop streams into it, and Session Sync's Wake step
    /// above refused to touch it), and a terminal card keeps the ending it
    /// recorded. The refresh can NEVER post a card
    /// ([`SplitPolicy::Forbid`](flush::SplitPolicy::Forbid)): a ledger-only
    /// change re-renders the whole live slice on the same card — tail included
    /// — instead of finalizing it and continuing the chain, so the acceptance
    /// "no new card" holds even for a card whose estimate was near the split
    /// budget. A card a half-finished handoff left behind (a finalized slice
    /// whose continuation send failed, or a queued split) is NOT refreshed:
    /// that handoff owns the chain's next write, and this pass must not post
    /// the card it owes. Runs under the session's card-write lock
    /// ([`CardsHandle::write_lock`]), like every other card write, so the facts
    /// and the PATCH they owe cannot interleave with a split, a collect or
    /// another flush. `now_ms` is the read's clock, shared with the same pass's
    /// Wake decision. Returns what this read did — the card is PATCHed exactly
    /// when it moved.
    ///
    /// The live subagents' child liveness (spec #501) is gathered on the same
    /// pass — one transcript light read per distinct child plus its pending-wait
    /// query, through the shared batch the live task panel uses — BEFORE the
    /// write lock (the reads are network calls) and only while a card that
    /// admits the refresh exists: a session with no such card, no live
    /// subagent, or none at all (V1) gathers nothing and spends no request.
    /// The batch shares the pass's read bound (`read_timeout_ms`) like the
    /// parent transcript read: one child on a half-open connection must not
    /// freeze Session Sync for every session, and a timed-out batch leaves each
    /// row exactly as it was (its stored fragment, ADR-0054).
    #[allow(clippy::too_many_arguments)] // the read's facts + the pass's read bound
    pub(crate) async fn refresh_yielded_ledger(
        cards: &CardsHandle,
        backend: &Arc<dyn crate::backend::Backend>,
        requests: &RequestsHandle,
        session_id: &str,
        transcript: &SessionTranscript,
        now_ms: i64,
        stopped: bool,
        read_timeout_ms: u64,
    ) -> YieldedUpdate {
        // The read-time form of the yielded-card write admission: whether this
        // pass should spend the child-liveness reads at all. The write below
        // re-checks the same rule under the card-write lock. The completion
        // entries this read owes are planned here too (spec #593) — on the
        // same admission, so a card this pass cannot serve spends neither the
        // child reads nor any output read — and their one output read each
        // runs outside the cards lock below.
        let (admitted, mut plans) = {
            let live = cards.cards.lock().await;
            match live.get(session_id) {
                Some(card) if ownership::admits_ledger_refresh(card) => (
                    true,
                    render::plan_ledger_entries(&card.acc, transcript, card.acc.turn_anchor.as_ref()),
                ),
                _ => (false, Vec::new()),
            }
        };
        render::read_planned_outputs(backend, &mut plans).await;
        let activities = if admitted {
            let children = render::background_subagent_children(transcript);
            if children.is_empty() {
                HashMap::new()
            } else {
                crate::bridge::bounded_call("yielded ledger child liveness", read_timeout_ms, async {
                    Ok::<_, crate::error::BridgeError>(
                        render::gather_child_liveness(backend, requests, &children).await,
                    )
                })
                .await
                .and_then(Result::ok)
                .unwrap_or_default()
            }
        } else {
            HashMap::new()
        };
        let write_lock = cards.write_lock(session_id).await;
        let _guard = write_lock.lock().await;
        let (changed, settled, notice_at, stamped_card) = {
            // The same admission, re-checked under the write lock through the
            // module's lock-scoped helper: the returned card is still locked,
            // so the check and the write below share one lock.
            let Some(mut card) = CardOwnership::admit_ledger_write(cards, session_id).await else {
                return YieldedUpdate::Unchanged;
            };
            let anchor = card.acc.turn_anchor.clone();
            let changed = render::apply_ledger_read(
                &mut card.acc,
                transcript,
                &activities,
                now_ms,
                state::LedgerCadence::Second,
                plans,
            );
            // The read's own settle decision judges the true end (ADR-0059):
            // only a read whose Wakes are answered and that retired the last
            // Background Task settles, and the ending is the one disposition
            // table's quiet mapping (#540): a settled failure or a deliberate
            // stop dominates ✅. A Waiting card always carries an anchor (the
            // yield is only decided from one), so the anchorless Unreceived
            // decision cannot arise here; a Running read keeps observing.
            let settled = Disposition::of_quiet_settle(transcript.settle(anchor.as_ref()), stopped);
            if let Some(disposition) = &settled {
                card.acc.apply_ending(disposition);
            }
            // A settled Wake continuation owes no notice: its own card send was
            // the notification (ADR-0059). The Turn's own waiting card carries
            // its start as the long-task clock.
            let notice_at = if card.acc.wake_continuation() {
                None
            } else {
                card.acc.turn_started_at()
            };
            // The card the ending landed on (spec #602, review finding 6):
            // captured under the same lock as the stamp, so the caller's notice
            // is bound to it.
            (changed, settled, notice_at, card.card_message_id.clone())
        };
        if let Some(disposition) = settled {
            // The footer is refreshed one last time (ADR-0019): the card
            // stopped updating at the yield, and the work behind it may have
            // moved the branch or the tree. Under the same write lock as the
            // flush, so the refreshed footer and the ending ride one PATCH.
            // Best effort, like every other finalize. The flush's own entry
            // drops the card's now-spent durable record (ADR-0063).
            state::refresh_work_context(cards, session_id).await;
            let flush = flush::flush_card_locked(cards, session_id, flush::SplitPolicy::Forbid).await;
            tracing::info!("yielded card settled in place: session {session_id} ({disposition:?})");
            // The settle answer is the write's (review, PR #595): a
            // permanently refused ending never reached the card, so the caller
            // records nothing, announces nothing, and the task stays live for
            // a card that can render its entry.
            return match flush {
                FlushOutcome::Accepted => YieldedUpdate::Settled {
                    disposition,
                    notice_at,
                    card_message_id: stamped_card,
                },
                FlushOutcome::Refused => YieldedUpdate::Refused,
                // No write was issued (no card id): the card keeps what it
                // showed, like any other read that changed nothing.
                FlushOutcome::Unwritten => YieldedUpdate::Unchanged,
            };
        }
        if !changed {
            return YieldedUpdate::Unchanged;
        }
        // The guard (ADR-0060): a ledger-only refresh must never post a new
        // card, so this flush may not finalize and continue the chain.
        let flush = flush::flush_card_locked(cards, session_id, flush::SplitPolicy::Forbid).await;
        // `Refreshed` is reserved for an accepted write (review, PR #595): a
        // permanently refused payload leaves the card as it was, so the caller
        // records nothing.
        match flush {
            FlushOutcome::Accepted => YieldedUpdate::Refreshed,
            FlushOutcome::Refused => YieldedUpdate::Refused,
            FlushOutcome::Unwritten => YieldedUpdate::Unchanged,
        }
    }
}

/// The Turn's render interface (spec #298, A2b): the two operations sibling
/// flows invoke — the card subtitle built before a prompt, and the shared
/// render-and-flush a polled snapshot goes through. The poll loop, the part
/// rendering and the title refresh behind them are private to the `render`
/// submodule.
impl Turn {
    /// The session/thread name shown as the card subtitle, formatted as
    /// `<title> · <id-tail>` from the OpenCode server's own live session title
    /// (ADR-0007), fetched on demand.
    pub(crate) async fn session_subtitle(
        sessions: &SessionsHandle,
        backend: &Arc<dyn crate::backend::Backend>,
        thread_key: &ThreadKey,
        text: &str,
    ) -> String {
        render::session_subtitle(sessions, backend, thread_key, text).await
    }

    /// Render a polled Session Transcript into `session_id`'s live card,
    /// flushing when the content, header or context footer changed. Returns
    /// `None` when the session's accumulator vanished (the caller should stop);
    /// otherwise the pass's rendering facts together with the flush's write
    /// disposition ([`render::RenderPass::flush`]), which a reconcile caller
    /// gates its record on (review, PR #595).
    pub(crate) async fn render_and_flush(
        cards: &CardsHandle,
        sessions: &SessionsHandle,
        backend: &Arc<dyn crate::backend::Backend>,
        requests: &RequestsHandle,
        session_id: &str,
        transcript: &SessionTranscript,
    ) -> Option<render::RenderPass> {
        render::render_and_flush(cards, sessions, backend, requests, session_id, transcript).await
    }
}

/// The Instant Reminder facts a cola Turn's live card carries (ADR-0043): the
/// turn generation, the requester, and whether the prompt came from a group.
pub(crate) struct TurnPinSource {
    pub(crate) generation: u64,
    pub(crate) requester_open_id: String,
    pub(crate) is_group: bool,
}

/// The recovery fixture a terminal card carries for its one recovery action:
/// the Error card's retry (spec #391) or the Unreceived card's 重新发起
/// (#437). The original prompt, its reply target, and the identity/thread
/// facts a re-run reuses.
pub(crate) struct TurnRecovery {
    /// The failed/unreceived turn's session.
    pub(crate) session_id: String,
    pub(crate) prompt: String,
    pub(crate) reply_to: String,
    pub(crate) subtitle: String,
    pub(crate) requester_open_id: Option<String>,
    pub(crate) is_group: bool,
    pub(crate) cola_message_id: Option<String>,
}

impl TurnRecovery {
    /// This recovery as the new attempt's [`PromptContext`]: the same facts
    /// under the context's names, with the id policy the caller chose (`None`
    /// = a fresh `msg_cola_` id, `Some` = the previous attempt's). Images are
    /// not re-sent on a recovery (#391, out of scope).
    pub(crate) fn into_context(
        self,
        thread_key: ThreadKey,
        cola_message_id: Option<String>,
    ) -> PromptContext {
        PromptContext {
            session_id: self.session_id,
            thread_key,
            text: self.prompt,
            message_id: self.reply_to,
            subtitle: self.subtitle,
            is_retry: true,
            requester_open_id: self.requester_open_id,
            is_group: self.is_group,
            cola_message_id,
            images: Vec::new(),
            // A recovery never routes through the advisory read (its own
            // decision already judged the run): an ordinary card.
            advisory_live: false,
        }
    }
}

/// What a retry click decided to do, from the bounded status + transcript read
/// pair (spec #391). The matrix is evaluated in order: a live run first, then
/// the failed turn's transcript. The submit cells are generation-aware: id
/// reuse is only safe where the server contract says a same-id re-post of an
/// admitted turn still runs.
pub(crate) enum RetryDecision {
    /// The run is still alive (Busy/Retry): no prompt is submitted — the card
    /// is re-attached to the running run instead (ticket #393).
    Busy,
    /// Submit under a fresh `msg_cola_` id: the failed turn reads settled
    /// through the finalization's completion projection (`TurnView::complete` —
    /// a terminal finish on an assistant message within the turn), the
    /// submission was admitted but unfinished on a generation whose re-post is
    /// a no-op (V2), the decision read was unknown, or no id exists to reuse.
    NewId,
    /// Submit reusing the failed attempt's id. Two shapes only: nothing of the
    /// submission was stored (no anchor — both generations create and run it),
    /// or the id was admitted with an unfinished turn on a generation whose
    /// re-post continues it (V1's upsert). A same-id re-post of a SETTLED turn
    /// is always a fresh-id decision instead.
    Reuse(String),
}

/// The retry decision (spec #391's matrix), a pure function over the bounded
/// read pair so the cells are testable without a backend.
/// `reuse_continues_an_admitted_turn` is the server-contract capability
/// ([`crate::backend::Backend::reuse_continues_an_admitted_turn`]): true where
/// a same-id re-post of an admitted, unfinished turn still runs (V1), false
/// where admission makes it a no-op (V2).
///
/// - a live status (`SessionStatus::is_live`: Busy, or Retry's scheduled next
///   attempt) → [`RetryDecision::Busy`] (re-attach; no submit);
/// - a status read that failed, timed out or reported an unreadable kind is
///   unknown → a fresh id: the click must always have an effect;
/// - idle + no anchor for the id (nothing was admitted) → reuse (both
///   generations create and run it);
/// - idle + the failed turn reads settled through the finalization's own
///   neutral projection (`turn_for_user(..).complete`, i.e. a terminal finish
///   on an assistant message within the turn) → a fresh id (a same-id re-post
///   against a settled turn runs nothing, and the new attempt is the honest
///   history);
/// - idle + the id admitted but its turn unfinished (a failed step, or an
///   orphaned run): reuse where the capability is true (V1's upsert-continue),
///   a fresh id where it is false (V2's admission no-op);
/// - no id to reuse, or no transcript to read → a fresh id.
pub(crate) fn retry_decision(
    status: Option<SessionStatus>,
    transcript: Option<&SessionTranscript>,
    cola_message_id: Option<&str>,
    reuse_continues_an_admitted_turn: bool,
) -> RetryDecision {
    match status {
        // Unknown status: submit a fresh attempt rather than risk a no-op.
        None => RetryDecision::NewId,
        Some(status) => {
            // A live run (`is_live`: Busy, or Retry's scheduled next attempt)
            // is re-attached, never re-prompted.
            if status.is_live() {
                return RetryDecision::Busy;
            }
            // Idle: the failed turn's transcript decides the id policy.
            let Some(id) = cola_message_id else {
                return RetryDecision::NewId;
            };
            // An unreadable transcript is unknown too.
            let Some(transcript) = transcript else {
                return RetryDecision::NewId;
            };
            let Some(anchor) = transcript.anchor_of_user(id) else {
                // The submission stored no user message (or one that cannot be
                // ordered): nothing was admitted, so the idempotent reuse is
                // the safe submit on either generation.
                return RetryDecision::Reuse(id.to_string());
            };
            let settled = transcript.turn_for_user(&anchor).complete;
            if settled {
                // A same-id re-post against a settled turn runs nothing (V2)
                // or re-authors nothing useful (V1): take a fresh attempt.
                RetryDecision::NewId
            } else if reuse_continues_an_admitted_turn {
                // V1's upsert: the re-post continues the admitted turn.
                RetryDecision::Reuse(id.to_string())
            } else {
                // V2's admission key: the re-post would be a silent no-op, so
                // the retry must be a new attempt.
                RetryDecision::NewId
            }
        }
    }
}

/// How a resolution leaves its residue on the card's timeline (ADR-0038,
/// rule 4): one receipt per resolved block (a click), or ONE mode line for
/// every block a mode change resolved.
pub(crate) enum InlineResidue<'a> {
    /// One receipt per resolved block, naming that block's own target.
    PerBlock(&'a (dyn Fn(&str) -> String + Send + Sync)),
    /// ONE receipt for the whole resolution.
    Single(&'a str),
}

/// A card's re-arm transition: moves a card out of one ending (Error or
/// Unreceived) and yields the follow fixture's facts — the anchor it watches
/// and the directory its status reads route under — or `None` when the card is
/// no longer in that ending. Passed to [`Turn::rearm_and_follow`] as the one
/// fact distinguishing a retry re-attach from a resumed re-arm.
type CardRearm = fn(&mut state::CardSession, Option<String>) -> Option<(Option<TurnAnchor>, String)>;

/// The Turn's card-state interface (spec #298, A3): the operations sibling
/// flows invoke against a session's streaming card — identity, lifecycle,
/// interaction blocks and the external renderer's arming. The accumulator and
/// its card session are private to the `state` submodule; no bridge module
/// outside the Turn reads their fields, and every card update below is the
/// only path to the state behind them.
impl Turn {
    /// Whether the session currently has a card session at all.
    pub(crate) async fn has_card(cards: &CardsHandle, session_id: &str) -> bool {
        cards.cards.lock().await.contains_key(session_id)
    }

    /// Whether the session's card is still running (not terminal: Done, Error,
    /// Retried, Stopped, or a collected waiting card). The map's key alone
    /// does not mean a live card — a completed/stopped/collected turn stays
    /// until the next Turn replaces it.
    pub(crate) async fn is_running(cards: &CardsHandle, session_id: &str) -> bool {
        cards
            .cards
            .lock()
            .await
            .get(session_id)
            .is_some_and(state::CardSession::is_running)
    }

    /// The session's tracked card message id, if a card has been sent.
    pub(crate) async fn card_message_id(cards: &CardsHandle, session_id: &str) -> Option<String> {
        cards
            .cards
            .lock()
            .await
            .get(session_id)
            .and_then(|c| c.card_message_id.clone())
    }

    /// The message a card for this session should reply to, if a live turn owns
    /// one.
    pub(crate) async fn reply_target(cards: &CardsHandle, session_id: &str) -> Option<String> {
        cards
            .cards
            .lock()
            .await
            .get(session_id)
            .and_then(|c| c.acc.reply_to_message_id().map(str::to_string))
    }

    /// Seed `acc`'s Wake floor from the chain's durable Wake Watermark (spec
    /// #561, review #569; ADR-0061): every Wake at or below the mark was
    /// announced by an earlier card — the mark outlives the record it was
    /// announced on — so this card's render neither re-inserts its completion
    /// entry nor stages a watermark advance for it. A strictly newer Wake still
    /// renders and announces.
    fn seed_wake_floor(cards: &CardsHandle, session_id: &str, acc: &mut state::StreamAccumulator) {
        acc.set_wake_floor(cards.chains.announced(session_id).map(|mark| mark.created_ms));
    }

    /// Whether the session's card has rendered any part — the external
    /// renderer's "partial reply" probe before it finalizes on timeout.
    pub(crate) async fn has_rendered_content(cards: &CardsHandle, session_id: &str) -> bool {
        cards.cards.lock().await.get(session_id).is_some_and(|c| {
            !c.acc.rendered_parts.is_empty()
                || !c.acc.seeded_delivered.is_empty()
                || !c.acc.tools.is_empty()
                || c.acc.todo_panel.is_some()
        })
    }

    /// The accumulator's observable-progress serial (#457): bumped by every
    /// render stage that changes content, a panel revision, a live fragment,
    /// ledger rows, or context tokens — never by the header tick or a rendered
    /// clock number. The external renderer renews its idle bound when this
    /// advances; reading the card (not a pass's return value) means progress a
    /// timed-out pass had already rendered still counts.
    pub(crate) async fn progress_mark(cards: &CardsHandle, session_id: &str) -> Option<u64> {
        cards
            .cards
            .lock()
            .await
            .get(session_id)
            .map(|c| c.acc.progress_mark())
    }

    /// Whether the session's card still carries an unfinished Tool Panel the
    /// Turn owns — a call whose status is `running` or `pending` (both render
    /// `⏳`). The drain follow's Done decision waits for these to settle (#284):
    /// the card must never read `✅ 完成` over a `⏳` panel. A still-running
    /// **seeded** call (spec #561's live set, ADR-0068's successor) is
    /// deliberately not one of the Turn's own: it is display-only, so it must
    /// not extend the settle decision — a seeded call the Turn's window never
    /// renders stops being counted the moment it was seeded, and the settled
    /// card omits it instead of waiting on it. A seeded call the window DID
    /// render has already left the seeded set, so today's guard applies to it
    /// as before.
    pub(crate) async fn has_live_tools(cards: &CardsHandle, session_id: &str) -> bool {
        cards.cards.lock().await.get(session_id).is_some_and(|c| {
            c.acc.tools.iter().any(|(call_id, panel)| {
                !c.acc.seeded_calls.contains(call_id)
                    && crate::feishu::card::tool_render::ToolPanel::is_live(panel)
            }) || c
                .acc
                .todo_panel
                .as_ref()
                .is_some_and(crate::feishu::card::tool_render::ToolPanel::is_live)
        })
    }

    /// Re-point the live card identity at a new message (ADR-0028: a re-adopt
    /// mid-turn sends a fresh snapshot; the follow renderer keeps updating the
    /// new card instead of the old one). The content is untouched. The durable
    /// record follows the re-point: the new card is the session's live card now
    /// (the old snapshot was already static, so nothing is collected).
    pub(crate) async fn repoint_card(cards: &CardsHandle, session_id: &str, message_id: &str) {
        if let Some(card) = cards.cards.lock().await.get_mut(session_id) {
            card.repoint(message_id);
        }
        Self::track_live_card(cards, session_id, message_id, PredecessorCollect::Never, None).await;
    }

    /// Refresh the live card's work context at turn end (ADR-0019): re-read the
    /// session directory's git state so the final card shows where the turn
    /// landed.
    pub(crate) async fn refresh_work_context(cards: &CardsHandle, session_id: &str) {
        state::refresh_work_context(cards, session_id).await;
    }

    /// Apply an ending `disposition` only while the card's accumulator is
    /// still `anchor`'s — the check and the application share ONE lock, so a
    /// renderer whose bound fired after a successor replaced the accumulator
    /// can never stamp the successor's live card (#457). Reads the anchor the
    /// external arm stored on the accumulator. Returns whether it applied
    /// (false: a successor owns the card now — nothing is touched).
    /// The anchor-guarded ending application, for callers that decided their own
    /// disposition through the one table: an external render loop's settle
    /// mapping (Waiting/Failed) applies here, beside the Done and Stopped
    /// endpoints below, so the successor guard (`false`: a successor owns the
    /// card — nothing is touched) and the flush are shared by every ending.
    pub(crate) async fn apply_disposition_if_anchor(
        cards: &CardsHandle,
        session_id: &str,
        anchor: &TurnAnchor,
        disposition: &Disposition,
    ) -> bool {
        let stamped = {
            let mut live = cards.cards.lock().await;
            match live.get_mut(session_id) {
                Some(card) if card.acc.turn_anchor.as_ref() == Some(anchor) => {
                    card.acc.apply_ending(disposition);
                    true
                }
                _ => false,
            }
        };
        if !stamped {
            return false;
        }
        Self::refresh_work_context(cards, session_id).await;
        Self::flush_card(cards, session_id).await;
        true
    }

    /// The anchor-guarded `Done` terminal: stamp the card Done through the one
    /// ending application, behind [`Self::apply_disposition_if_anchor`]'s
    /// guard. Returns whether it finalized (false: a successor owns the card
    /// now — nothing is touched).
    pub(crate) async fn finalize_done_if_anchor(
        cards: &CardsHandle,
        session_id: &str,
        anchor: &TurnAnchor,
    ) -> bool {
        Self::apply_disposition_if_anchor(cards, session_id, anchor, &Disposition::Done).await
    }

    /// The stop terminal under the same anchor guard as
    /// [`Self::finalize_done_if_anchor`] (#457): a `/stop` seen by a stale
    /// renderer must not stamp its successor's card.
    pub(crate) async fn finalize_stopped_if_anchor(
        cards: &CardsHandle,
        session_id: &str,
        anchor: &TurnAnchor,
    ) -> bool {
        Self::apply_disposition_if_anchor(cards, session_id, anchor, &Disposition::Stopped).await
    }

    /// Collect `session_id`'s Waiting card (ADR-0059, spec #405): restamp it
    /// with the reason's collected header — 「⏳ 部分完成 · 已由新消息接管」 for a
    /// supersede, 「⏳ 已切换会话 · 后台任务仍在运行」 for a Session that stopped
    /// being the thread's Active Session — refresh the work context and flush
    /// once. The collect also drops the card's live ledger section (ADR-0060):
    /// a card that stops updating cannot carry a list that claims to be live.
    /// No-op — nothing flushed — when the session has no card or the card
    /// is not waiting. Neither collect sends a Completion Notice (the notice
    /// belongs to a true end), and the background work behind the wait is
    /// unaffected: a later Wake still continues the chain on a new card.
    ///
    /// `pub(crate)` for the mapping operations on
    /// [`FlowHandles`](crate::bridge::handles::FlowHandles), which is where the
    /// switch-away collect is an invariant (spec #405).
    pub(crate) async fn collect_waiting(
        cards: &CardsHandle,
        session_id: &str,
        reason: CollectReason,
    ) -> bool {
        let collected = {
            let mut live = cards.cards.lock().await;
            live.get_mut(session_id)
                .is_some_and(|card| card.acc.collect_waiting(reason.collected()))
        };
        if !collected {
            return false;
        }
        tracing::info!("card collect: session {session_id} collected ({reason:?})");
        Self::refresh_work_context(cards, session_id).await;
        Self::flush_card(cards, session_id).await;
        true
    }

    /// Collect every Waiting card of `thread_key`'s mapped sessions because a
    /// new Turn in the thread superseded them (ADR-0059, spec #405): the new
    /// message is the thread's newest word, so no waiting card it displaces may
    /// keep sitting on 「⏳ 等待后台任务」. The Turn being started is included —
    /// its own accumulator is replaced right after this runs.
    async fn collect_thread_waiting(cards: &CardsHandle, sessions: &SessionsHandle, thread_key: &ThreadKey) {
        for session_id in sessions.session_ids_for_thread(thread_key).await {
            Self::collect_waiting(cards, &session_id, CollectReason::Superseded).await;
        }
    }

    /// `request_id → card_message_id` for every live block a card session's
    /// accumulator still owns. The sweep uses it to leave those blocks to their
    /// own flush (ADR-0038, rule 2).
    pub(crate) async fn flush_owned_blocks(cards: &CardsHandle) -> HashMap<String, String> {
        let live = cards.cards.lock().await;
        let mut owned = HashMap::new();
        for card in live.values() {
            let Some(message_id) = card.card_message_id.as_deref() else {
                continue;
            };
            for id in card.acc.live_request_ids() {
                owned.insert(id.to_string(), message_id.to_string());
            }
        }
        owned
    }

    /// Claim a terminal card's recovery action exactly once: spec #391's Error
    /// retry (`from = CardState::Error`) or #437's Unreceived 重新发起
    /// (`from = CardState::Unreceived`) — [`CardState::offers_recovery`] is the
    /// one predicate naming the states a caller may pass. Returns the turn's
    /// recovery fixture while the card is still in `from` and no earlier click
    /// holds the claim; `None` when `from` names no action, the card is in
    /// another state (already marked, live, or replaced), a claim is already
    /// taken, no card exists, or no prompt was stored to re-run (an
    /// externally-rendered card). This atomic claim is what makes the callback
    /// safe to double-click: the click is acked immediately, so a second click
    /// can arrive before the action's own marking lands.
    pub(crate) async fn claim_recovery(
        cards: &CardsHandle,
        session_id: &str,
        from: crate::feishu::card::CardState,
    ) -> Option<TurnRecovery> {
        if !from.offers_recovery() {
            return None;
        }
        let mut live = cards.cards.lock().await;
        let card = live.get_mut(session_id)?;
        if *card.acc.card_state() != from
            || card.acc.recovery_claimed
            // A Wake continuation carries no question to re-ask (ADR-0059):
            // its card never offers Retry, so no click may claim one either.
            || card.acc.wake_continuation()
        {
            return None;
        }
        // A card that was never sent cannot be marked; an Error card always
        // has one (`Turn::start` drops the session when the Loading reply
        // fails).
        card.card_message_id.as_ref()?;
        // An external turn's ending has no prompt to re-submit; the button
        // must not claim (there is nothing to recover).
        let prompt = card
            .acc
            .prompt()
            .filter(|prompt| !prompt.is_empty())
            .map(str::to_string)?;
        card.acc.recovery_claimed = true;
        Some(TurnRecovery {
            session_id: session_id.to_string(),
            prompt,
            reply_to: card.acc.reply_to_message_id().unwrap_or_default().to_string(),
            subtitle: card.acc.title().to_string(),
            requester_open_id: card.acc.requester_open_id().map(str::to_string),
            is_group: card.acc.is_group(),
            cola_message_id: card.acc.cola_message_id().map(str::to_string),
        })
    }

    /// Claim a Waiting card's cleanup click (spec #588, ticket #590) — the
    /// same atomic double-click guard the recovery actions use, on the wait's
    /// own home. The claim is taken only when the card is Waiting, was sent,
    /// owes no split, and still carries at least one unconfirmed ledger row —
    /// the very predicate the button's render reads, plus the write
    /// admission's own liveness/handoff conditions, so a click on a card the
    /// pipeline could not write is refused rather than recording a clearance
    /// whose 🧹 entry would have nowhere to land. The click is acked
    /// immediately, so the claim is what keeps a second click from running a
    /// second cleanup; [`Self::release_recovery_claim`] gives it back when the
    /// pipeline is done (or could not proceed).
    pub(crate) async fn claim_cleanup(cards: &CardsHandle, session_id: &str) -> bool {
        let mut live = cards.cards.lock().await;
        let Some(card) = live.get_mut(session_id) else {
            return false;
        };
        if *card.acc.card_state() != crate::feishu::card::CardState::Waiting
            || card.acc.recovery_claimed
            || card.card_message_id.is_none()
            || !card.card_is_live
            || !card.pending_split.is_empty()
            || !card.acc.ledger.iter().any(|row| row.unconfirmed)
        {
            return false;
        }
        card.acc.recovery_claimed = true;
        true
    }

    /// Give an unused cleanup claim back (spec #588, #590): the recovery
    /// claim's own release, named for its caller.
    pub(crate) async fn release_cleanup_claim(cards: &CardsHandle, session_id: &str) {
        Self::release_recovery_claim(cards, session_id).await;
    }

    /// Release an unused recovery claim. A click that neither submits nor
    /// re-attaches (the `Busy` decision with no anchor to follow, a retry that
    /// lost the inflight guard, a resume whose write failed, or a vanished
    /// session/thread) gives the claim back, so a later click can try again
    /// instead of finding a dead button.
    pub(crate) async fn release_recovery_claim(cards: &CardsHandle, session_id: &str) {
        if let Some(card) = cards.cards.lock().await.get_mut(session_id) {
            card.release_recovery_claim();
        }
    }

    /// The shared body of a recovery re-arm (spec #391's retry re-attach, #437's
    /// resumed re-arm): `rearm` moves the card out of its ending and returns
    /// the follow fixture — the anchor it watches (`None` for the Unreceived
    /// watch: the message never landed) and the directory its status reads
    /// route under — or `None` when the card is no longer in that ending (a new
    /// Turn may have replaced it since the claim). The new live state is
    /// flushed now, so the operator's click is visible before the follow's
    /// first sleep, then the out-of-turn [`follow`] is spawned: it takes the
    /// Session's guard — this path held none (ADR-0059) — and its window owns
    /// Busy→non-busy finalization, `/stop`, the graces and the silent exit when
    /// a new Turn replaces the accumulator.
    ///
    /// `directory` routes the follow's status reads (the caller's
    /// session-mapped one first, the accumulator's work context as the
    /// fallback). The follow's start time is "now": the original turn's start is
    /// no longer known here, so the long-task notice measures the re-attached
    /// stretch.
    ///
    /// Returns false when `rearm` declined — the caller then releases the claim.
    async fn rearm_and_follow(
        handles: &TurnHandles,
        session_id: &str,
        thread_key: &ThreadKey,
        directory: Option<String>,
        rearm: CardRearm,
    ) -> bool {
        let Some((anchor, directory)) = ({
            let mut live = handles.cards.cards.lock().await;
            live.get_mut(session_id).and_then(|card| rearm(card, directory))
        }) else {
            return false;
        };
        // The card is live again: its durable record comes back with it (the
        // terminal that owned the ending dropped it), so a crash during the
        // re-armed run still reaps.
        let card_message_id = Self::card_message_id(&handles.cards, session_id).await;
        if let Some(card_message_id) = card_message_id {
            Self::track_live_card(
                &handles.cards,
                session_id,
                &card_message_id,
                PredecessorCollect::Never,
                None,
            )
            .await;
        }
        Self::flush_card(&handles.cards, session_id).await;
        follow::spawn(
            handles,
            follow::FollowFacts {
                session_id: session_id.to_string(),
                thread_key: thread_key.clone(),
                directory,
                started_at: std::time::Instant::now(),
                anchor,
            },
        )
        .await;
        true
    }

    /// Re-attach a still-running run to its live card (spec #391, ticket #393):
    /// the retry click's decision read found the run alive, so nothing is
    /// submitted. The card's Error is cleared — its content is untouched — the
    /// unused retry claim goes back (the follow may fail the card again, and
    /// that retry must be claimable), and the out-of-turn [`follow`] is spawned
    /// on the accumulator's own anchor: both the flush and the guard hand-off
    /// live in [`Self::rearm_and_follow`].
    ///
    /// Returns false when there is no card, the card is not in `Error` (a new
    /// Turn may have replaced the accumulator since the claim), or the
    /// accumulator carries no anchor to follow (nothing the failed submission
    /// stored can be ordered against a run) — the caller then releases the
    /// claim and the Error card keeps a working retry.
    pub(crate) async fn reattach_run(
        handles: &TurnHandles,
        session_id: &str,
        thread_key: &ThreadKey,
        directory: Option<String>,
    ) -> bool {
        Self::rearm_and_follow(handles, session_id, thread_key, directory, CardSession::reattach).await
    }

    /// Re-arm an Unreceived card after a successful V2 resume (#437): the
    /// resumed run renders on the SAME card, through the unreceived watch —
    /// the out-of-turn [`follow`] under the card's chain identity (its
    /// accumulator anchor is `None`: the submitted message never landed),
    /// which captures the Turn anchor the moment the promoted message appears
    /// and then settles the card through the single decision. The flush and
    /// the guard hand-off live in [`Self::rearm_and_follow`].
    ///
    /// Returns false when there is no card, or the card is no longer
    /// `Unreceived` (a new Turn may have replaced it since the claim) — the
    /// caller then releases the claim, and a later press can retry.
    pub(crate) async fn reattach_resumed(
        handles: &TurnHandles,
        session_id: &str,
        thread_key: &ThreadKey,
        directory: Option<String>,
    ) -> bool {
        Self::rearm_and_follow(
            handles,
            session_id,
            thread_key,
            directory,
            CardSession::rearm_unreceived,
        )
        .await
    }

    /// Mark the session's failed card `Retried` and flush it (spec #391):
    /// header 「↩️ 已重试」, retry button suppressed, the marker line appended,
    /// all failed content preserved. Called by `Turn::start`'s retry path once
    /// the inflight guard is held — never before, so a retry that loses the
    /// guard cannot leave the card marked without a new attempt. A failed
    /// PATCH only warns inside the flush — the retry proceeds on its new card
    /// regardless.
    async fn mark_retried(cards: &CardsHandle, session_id: &str) {
        if let Some(card) = cards.cards.lock().await.get_mut(session_id) {
            card.acc.mark_retried();
        }
        Self::flush_card(cards, session_id).await;
    }
}

impl Turn {
    /// Attach a sent card's identity to an armed-but-idless continuation and
    /// re-point its durable record without collecting a predecessor — the card
    /// continues the chain it already owns (ADR-0059, ADR-0063). The test seam
    /// for tests that name a seeded card; production takeover paths use
    /// [`Self::take_over_card`], which attaches and collects in one ordered
    /// step.
    #[cfg(test)]
    pub(crate) async fn set_card_message_id(cards: &CardsHandle, session_id: &str, message_id: &str) {
        Self::attach_card_message_id(cards, session_id, message_id).await;
        Self::track_live_card(cards, session_id, message_id, PredecessorCollect::Never, None).await;
    }
}

/// Send the Completion Notice (ADR-0043 amendment 2026-09-21) gated on the
/// terminal write's own **fate** (spec #602, ticket #607): the streaming card is
/// patched in place, which pushes no notification and does not bump the
/// conversation — so reply to the requester's message to notify them. Groups
/// notify on every turn (`[bridge] group_completion_notice`); p2p only for a
/// long task (`[bridge] long_task_notice`, past the injected threshold), where
/// "long" is the one event worth surfacing even though the user was presumably
/// around. The copy follows the ending's [`Disposition`] (#394): ✅ for the true
/// end, ⏹ for a deliberate `/stop`, ❌ for a failure — and the disposition's
/// classification is the refusal, so a Waiting yield (whose true end is not
/// reached, ADR-0059) and an Unreceived ending (ADR-0062) can never be announced
/// even by a caller that forgot to guard. A free function because every end of a
/// turn calls it (`finish`, the out-of-turn follow, the Wake in-place resume and
/// Session Sync's quiet true end); `finish` keeps the ORIGINAL turn's start, so
/// the long-task threshold measures the whole run.
///
/// The write must have been delivered now, or be owed by the
/// delivery layer's retry. A direct PATCH to the current card and a size-split
/// that continued on a new card both read [`EndingWrite::Delivered`] and
/// announce at once; a failed ending PATCH queued as a Pending Card Update reads
/// [`EndingWrite::Owed`] and arms the notice against that **exact sequence**, so
/// it fires only when that write DRAINS — never before, and never on an
/// unrelated newer repaint. A permanently refused terminal write — and a
/// size-split continuation create that failed, whose slice never reached Feishu
/// — reads [`EndingWrite::Failed`] and suppresses the notice. A missing tracked
/// card is likewise suppressed: without a card there is no write to gate on. The
/// disposition's own classification and the opt-in rules are unchanged — this
/// only fixes the timing/gate.
///
/// The one entry point every ending takes (the in-Turn `finish`, the out-of-turn
/// follow, the Wake in-place resume and Session Sync's quiet true end) — so all
/// carriers are covered by one rule. The recipient and copy are resolved ONCE
/// here, from [`CapturedNotice`] — the card id, ending fate and request identity
/// read in a SINGLE critical section. `expected_card` is the identity of the
/// card the ending was APPLIED to (the id the stamp returned, `None` when the
/// card carries no id); a session whose current card differs from it has had
/// that card replaced in the released gap between the stamp and this capture, so
/// the notice is suppressed rather than announced on the successor. A deferred
/// notice additionally carries the
/// ORIGINAL request's requester and re-checks the card's identity before sending
/// ([`deliver_deferred_notice`]), so a newer Turn's card is never announced over
/// the old request's ending (review findings 1, 5 and 6).
pub(crate) async fn announce_completion(
    cards: &CardsHandle,
    platform: &Arc<dyn crate::feishu::Platform>,
    rules: &NoticeRules,
    session_id: &str,
    started_at: std::time::Instant,
    disposition: &Disposition,
    expected_card: Option<&str>,
) {
    // Not an ending the notice may announce: stay silent before any gate (the
    // primitive would decline anyway; this keeps the deferred path quiet too).
    if disposition.notice_copy().is_none() {
        return;
    }
    // The card carrying the terminal slice, that write's own fate (#607) AND
    // the request's identity, read under ONE lock (spec #602, review findings
    // 5 and 6): a newer Turn that replaces the card must never redirect the
    // notice, so the recipient, chat kind and copy are resolved against the
    // SAME card identity the terminal write was gated on. The ending was
    // applied to `expected_card`; a session whose CURRENT card is a different
    // identity has had that card replaced (or collected) in the released gap
    // between the stamp and this capture — the old ending must not ride the
    // replacement's (default-`Delivered`) accumulator. `Failed` when a
    // size-split continuation create failed or an ending PATCH was permanently
    // refused — the tail never reached Feishu; `Owed(seq)` when delivery still
    // owes that exact write.
    let captured = {
        let live = cards.cards.lock().await;
        match live.get(session_id) {
            Some(card) if card.card_message_id.as_deref() == expected_card => Some(CapturedNotice {
                card_message_id: card.card_message_id.clone(),
                ending: card.acc.ending_write(),
                requester: card.acc.requester_open_id().map(str::to_string),
                reply_to: card.acc.reply_to_message_id().map(str::to_string),
                is_group: card.acc.is_group(),
            }),
            // A card exists but is NOT the one the ending was applied to: a
            // newer Turn replaced it (or a collect took it). Announcing the old
            // request's ending on it would notify the wrong requester.
            Some(card) => {
                tracing::warn!(
                    "completion notice suppressed: the card for {session_id} was replaced after the ending (applied to {:?}, current {:?})",
                    expected_card,
                    card.card_message_id,
                );
                None
            }
            None => None,
        }
    };
    // No tracked card: there is no write to gate on, so the notice is
    // suppressed rather than sent without a delivery verdict (spec #602,
    // review). A card collected before its ending has no terminal write that
    // could have reached Feishu.
    let Some(captured) = captured else {
        tracing::warn!("completion notice suppressed: no tracked card for {session_id} to gate delivery on");
        return;
    };
    // No card id: same suppression — without one there is no write to gate on.
    let Some(card_id) = captured.card_message_id.clone() else {
        tracing::warn!("completion notice suppressed: no tracked card for {session_id} to gate delivery on");
        return;
    };
    // Resolve the notice's recipient, chat kind and copy against the CAPTURED
    // card — never a re-read (spec #602, review finding 5). A deferred notice
    // must send to THIS request's requester, never to a card a newer Turn
    // replaced while the terminal write was still owed. `None` keeps it silent
    // (the disposition, the opt-ins or a missing requester/reply target).
    let resolved = captured.resolve(session_id, rules, started_at, disposition);
    match captured.ending {
        // The terminal slice's own carrier refused: the delivery layer would
        // answer for the OLD card (whose earlier write delivered), so the
        // notice would be announced over a terminal tail that never landed.
        // Suppress instead.
        EndingWrite::Failed => {
            tracing::warn!("completion notice suppressed: the terminal write for {session_id} was refused");
        }
        // The terminal slice is on Feishu: announce at once, to the request that
        // ended.
        EndingWrite::Delivered => {
            if let Some(notice) = resolved {
                send_resolved_notice(platform, &notice).await;
            }
        }
        // The terminal slice is owed by the delivery layer's retry at THIS
        // sequence. The gate arms the notice against that exact sequence (so an
        // unrelated newer repaint can never answer for it); if the write
        // already drained it announces now, and if it was superseded or evicted
        // it is suppressed.
        EndingWrite::Owed(seq) => {
            let Some(notice) = resolved else {
                return;
            };
            // The deferred closure spawns the send on fire: the delivery layer
            // calls it from a drain, and the notice's own reply must not run
            // under the delivery layer's lock. It carries the resolved recipient
            // AND the original card's identity, so it re-checks ownership before
            // sending (review finding 1).
            let deferred: crate::feishu::DeferredNotice = {
                let cards = cards.clone();
                let platform = Arc::clone(platform);
                let notice = notice.clone();
                Box::new(move || {
                    tokio::spawn(async move {
                        deliver_deferred_notice(&cards, &platform, notice).await;
                    });
                })
            };
            match platform.defer_notice_until_delivered(&card_id, seq, deferred) {
                crate::feishu::NoticeGate::Armed => {}
                crate::feishu::NoticeGate::Delivered => {
                    send_resolved_notice(platform, &notice).await;
                }
                crate::feishu::NoticeGate::Never => {
                    tracing::warn!(
                        "completion notice suppressed: the terminal write for {session_id} will never land"
                    );
                }
            }
        }
    }
}

/// A Completion Notice resolved against the card that carried the terminal
/// write: the recipient, chat kind and copy fixed at arm time, plus the card
/// identity it belonged to (spec #602, review finding 1). Resolving once means
/// a deferred notice can never be redirected to a card a newer Turn replaced
/// while the terminal write was still owed.
#[derive(Clone)]
struct ResolvedNotice {
    session_id: String,
    card_message_id: String,
    reply_to: String,
    requester: String,
    is_group: bool,
    text: &'static str,
}

/// The card state `announce_completion` reads in ONE critical section (spec
/// #602, review finding 5): the card carrying the terminal write plus the
/// request's identity. Reading them together — never in a second lock — is what
/// binds a deferred notice to the request that ended, even if a newer Turn
/// replaces the card before the notice resolves.
struct CapturedNotice {
    card_message_id: Option<String>,
    ending: EndingWrite,
    requester: Option<String>,
    reply_to: Option<String>,
    is_group: bool,
}

impl CapturedNotice {
    /// Resolve the Completion Notice against THIS captured card, or `None` when
    /// the disposition, the opt-ins or a missing recipient keep it silent. It
    /// never re-reads the session, so the notice names exactly the request that
    /// ended.
    fn resolve(
        &self,
        session_id: &str,
        rules: &NoticeRules,
        started_at: std::time::Instant,
        disposition: &Disposition,
    ) -> Option<ResolvedNotice> {
        let text = disposition.notice_copy()?;
        if !(rules.group_completion_notice || rules.long_task_notice) {
            return None;
        }
        let card_message_id = self.card_message_id.clone()?;
        let requester = self.requester.clone()?;
        let reply_to = self.reply_to.clone()?;
        let long_task = started_at.elapsed() >= std::time::Duration::from_millis(rules.long_task_notice_ms());
        if !(self.is_group && rules.group_completion_notice
            || !self.is_group && rules.long_task_notice && long_task)
        {
            return None;
        }
        Some(ResolvedNotice {
            session_id: session_id.to_string(),
            card_message_id,
            reply_to,
            requester,
            is_group: self.is_group,
            text,
        })
    }
}

/// Send a resolved notice to its own recipient — the request that actually
/// ended — with the best-effort @-mention. On any lookup failure cola falls
/// back to a plain reply, which still notifies the message author; p2p needs no
/// @ because the reply itself is the notification.
async fn send_resolved_notice(platform: &Arc<dyn crate::feishu::Platform>, notice: &ResolvedNotice) {
    let name = if notice.is_group {
        platform.user_name(&notice.requester).await.unwrap_or(None)
    } else {
        None
    };
    if let Err(e) = platform
        .reply_completion_notice(&notice.reply_to, &notice.requester, name.as_deref(), notice.text)
        .await
    {
        tracing::warn!("completion notice: {}", e);
    }
}

/// Fire a deferred Completion Notice, but only if the session's card is STILL
/// the one the notice was armed against (spec #602, review finding 1): a newer
/// Turn that replaced the card must not have the OLD request's ending announced
/// to its requester. A missing card is a replacement/collect too — suppress.
async fn deliver_deferred_notice(
    cards: &CardsHandle,
    platform: &Arc<dyn crate::feishu::Platform>,
    notice: ResolvedNotice,
) {
    let current = {
        let live = cards.cards.lock().await;
        live.get(&notice.session_id)
            .and_then(|c| c.card_message_id.clone())
    };
    if current.as_deref() != Some(notice.card_message_id.as_str()) {
        tracing::warn!(
            "completion notice suppressed: the card for {} was replaced before its terminal write drained",
            notice.session_id
        );
        return;
    }
    send_resolved_notice(platform, &notice).await;
}

/// The Turn's test seam (spec #298, A3): the fixtures tests outside the Turn
/// need to exercise a flow on a card — seed a card session, feed it parts, and
/// read back what the flow rendered. The accumulator stays private; these
/// helpers are the test side of the same interface the production flows use.
/// The accumulator's own internal-seam tests live next to it in `state.rs`.
#[cfg(test)]
impl Turn {
    /// Drop the session's card session (a test teardown).
    pub(crate) async fn drop_card(cards: &CardsHandle, session_id: &str) {
        cards.cards.lock().await.remove(session_id);
    }

    /// Seed an empty card session for `session_id`.
    pub(crate) async fn seed_card(cards: &CardsHandle, session_id: &str, card_message_id: Option<&str>) {
        cards.cards.lock().await.insert(
            session_id.to_string(),
            state::CardSession::new(
                state::StreamAccumulator::new("test"),
                card_message_id.map(str::to_string),
            ),
        );
    }

    /// The session's staged Rendered Cursor (spec #561, review #569): the body
    /// most recently built and not yet confirmed.
    pub(crate) async fn staged_cursor(cards: &CardsHandle, session_id: &str) -> Option<RenderedCursor> {
        cards
            .cards
            .lock()
            .await
            .get(session_id)
            .and_then(|card| card.acc.last_staged_cursor().map(|staged| staged.cursor.clone()))
    }

    /// Stage a Rendered Cursor directly (spec #561, review #569) — a test seam
    /// for the confirmation's identity requirement, standing in for the flush
    /// that would stage it. Returns the stage identity a confirmation names.
    pub(crate) async fn stage_cursor(
        cards: &CardsHandle,
        session_id: &str,
        card_message_id: Option<&str>,
        cursor: &RenderedCursor,
    ) -> state::StagedCursorId {
        match cards.cards.lock().await.get_mut(session_id) {
            Some(card) => state::StagedCursorId {
                id: card.acc.stage_cursor(card_message_id, cursor.clone(), None),
                awaiting_seq: None,
            },
            None => state::StagedCursorId {
                id: 0,
                awaiting_seq: None,
            },
        }
    }

    /// Set the card's display state.
    pub(crate) async fn set_card_state(
        cards: &CardsHandle,
        session_id: &str,
        state: crate::feishu::card::CardState,
    ) {
        if let Some(card) = cards.cards.lock().await.get_mut(session_id) {
            card.acc.set_card_state(state);
        }
    }

    /// The identity of the NEWEST staged cursor (spec #561, review #569): the
    /// exact stage a confirmation must name, for the tests that drive the
    /// confirmation seams directly.
    pub(crate) async fn staged_cursor_id(
        cards: &CardsHandle,
        session_id: &str,
    ) -> Option<state::StagedCursorId> {
        cards.cards.lock().await.get(session_id).and_then(|card| {
            card.acc.last_staged_cursor().map(|staged| state::StagedCursorId {
                id: staged.id,
                awaiting_seq: staged.awaiting_seq,
            })
        })
    }

    /// Clear the card's phase timer (a fixture that needs a frozen header: a
    /// live card's timer otherwise ticks the header once a second).
    pub(crate) async fn clear_phase(cards: &CardsHandle, session_id: &str) {
        if let Some(card) = cards.cards.lock().await.get_mut(session_id) {
            card.acc.current_phase = None;
            card.acc.phase_started_at = None;
        }
    }

    /// Set the card's subtitle/title.
    pub(crate) async fn set_title(cards: &CardsHandle, session_id: &str, title: &str) {
        if let Some(card) = cards.cards.lock().await.get_mut(session_id) {
            card.acc.set_title(title);
        }
    }

    /// Set the card's reply target.
    pub(crate) async fn set_reply_target(cards: &CardsHandle, session_id: &str, reply_to: &str) {
        if let Some(card) = cards.cards.lock().await.get_mut(session_id) {
            card.acc.set_reply_target(Some(reply_to.to_string()));
        }
    }

    /// Set the card's Turn anchor to one the caller already holds (the test
    /// seam); production fills it from the transcript via
    /// `capture_turn_anchor`.
    pub(crate) async fn set_turn_anchor(cards: &CardsHandle, session_id: &str, anchor: &TurnAnchor) {
        if let Some(card) = cards.cards.lock().await.get_mut(session_id) {
            card.acc.turn_anchor = Some(anchor.clone());
        }
    }

    /// Record the turn's requester, chat type and generation.
    pub(crate) async fn set_turn_identity(
        cards: &CardsHandle,
        session_id: &str,
        requester_open_id: &str,
        is_group: bool,
        generation: u64,
    ) {
        if let Some(card) = cards.cards.lock().await.get_mut(session_id) {
            card.acc.set_requester(Some(requester_open_id.to_string()));
            card.acc.set_is_group(is_group);
            card.acc.set_turn_generation(generation);
        }
    }

    /// Record the answering model and its provider.
    pub(crate) async fn set_model(cards: &CardsHandle, session_id: &str, provider: &str, model: &str) {
        if let Some(card) = cards.cards.lock().await.get_mut(session_id) {
            card.acc.apply_footer_model(model, provider, None);
        }
    }

    /// Append text to the card's timeline.
    pub(crate) async fn push_text(cards: &CardsHandle, session_id: &str, text: &str) {
        if let Some(card) = cards.cards.lock().await.get_mut(session_id) {
            card.acc.push_text(text);
        }
    }

    /// Append reasoning to the card's timeline.
    pub(crate) async fn push_reasoning(cards: &CardsHandle, session_id: &str, text: &str) {
        if let Some(card) = cards.cards.lock().await.get_mut(session_id) {
            card.acc.push_reasoning(text);
        }
    }

    /// Append reasoning keyed at a server time.
    pub(crate) async fn push_reasoning_at(cards: &CardsHandle, session_id: &str, at_ms: i64, text: &str) {
        if let Some(card) = cards.cards.lock().await.get_mut(session_id) {
            card.acc.push_reasoning_at(Some(at_ms), text);
        }
    }

    /// Push a tool panel onto the card's timeline.
    pub(crate) async fn push_tool(
        cards: &CardsHandle,
        session_id: &str,
        call_id: &str,
        panel: crate::feishu::card::tool_render::ToolPanel,
    ) {
        if let Some(card) = cards.cards.lock().await.get_mut(session_id) {
            card.acc.push_tool(call_id, panel);
        }
    }

    /// Push a tool panel keyed at a server time.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn push_tool_at(
        cards: &CardsHandle,
        session_id: &str,
        at_ms: i64,
        call_id: &str,
        panel: crate::feishu::card::tool_render::ToolPanel,
    ) {
        if let Some(card) = cards.cards.lock().await.get_mut(session_id) {
            card.acc.push_tool_at(Some(at_ms), call_id, panel);
        }
    }

    /// The card's display state.
    pub(crate) async fn card_state(
        cards: &CardsHandle,
        session_id: &str,
    ) -> Option<crate::feishu::card::CardState> {
        cards
            .cards
            .lock()
            .await
            .get(session_id)
            .map(|c| c.acc.card_state().clone())
    }

    /// Whether the card's fallback reached the suspended state (the fenced
    /// retry was refused too): the flush's terminal mark, so a test can pin
    /// that a refused write leaves the card exactly as the suspension rules
    /// always did.
    pub(crate) async fn card_is_suspended(cards: &CardsHandle, session_id: &str) -> bool {
        cards
            .cards
            .lock()
            .await
            .get(session_id)
            .is_some_and(|c| c.acc.card_fallback() == state::CardFallback::Suspended)
    }

    /// The live permission blocks' `(request_id, session_id)`, in render order.
    pub(crate) async fn live_permissions(cards: &CardsHandle, session_id: &str) -> Vec<(String, String)> {
        cards
            .cards
            .lock()
            .await
            .get(session_id)
            .map(|c| {
                c.acc
                    .live_permissions()
                    .into_iter()
                    .map(|p| (p.request_id, p.session_id))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The live question blocks' `(request_id, answers)`, in render order.
    pub(crate) async fn live_questions(
        cards: &CardsHandle,
        session_id: &str,
    ) -> Vec<(String, Vec<Option<Vec<String>>>)> {
        cards
            .cards
            .lock()
            .await
            .get(session_id)
            .map(|c| {
                c.acc
                    .live_questions()
                    .into_iter()
                    .map(|q| (q.request_id, q.answers))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The card as it would render now.
    pub(crate) async fn rendered_card(cards: &CardsHandle, session_id: &str) -> Option<serde_json::Value> {
        cards
            .cards
            .lock()
            .await
            .get(session_id)
            .map(|c| c.acc.build_card())
    }

    /// The timeline index the card currently renders from.
    pub(crate) async fn render_from(cards: &CardsHandle, session_id: &str) -> Option<usize> {
        cards
            .cards
            .lock()
            .await
            .get(session_id)
            .map(|c| c.acc.render_from)
    }

    /// Whether the card session is the growing live card.
    pub(crate) async fn card_is_live(cards: &CardsHandle, session_id: &str) -> bool {
        cards
            .cards
            .lock()
            .await
            .get(session_id)
            .is_some_and(|c| c.card_is_live)
    }

    /// Whether the session owes a split continuation.
    pub(crate) async fn has_pending_split(cards: &CardsHandle, session_id: &str) -> bool {
        cards
            .cards
            .lock()
            .await
            .get(session_id)
            .is_some_and(|c| !c.pending_split.is_empty())
    }

    /// The queued splits' `(reply_to, receipt_pushed)`, in arrival order.
    pub(crate) async fn pending_splits(cards: &CardsHandle, session_id: &str) -> Vec<(String, bool)> {
        cards
            .cards
            .lock()
            .await
            .get(session_id)
            .map(|c| {
                c.pending_split
                    .iter()
                    .map(|s| (s.reply_to.clone(), s.receipt_pushed))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Whether any card carries a LIVE interaction block for `request_id`.
    pub(crate) async fn has_live_interaction(cards: &CardsHandle, request_id: &str) -> bool {
        cards.cards.lock().await.values().any(|c| {
            c.acc
                .interaction(request_id)
                .is_some_and(state::InteractionBlock::is_live)
        })
    }

    /// Whether any card carries an Interaction Receipt starting with `prefix`.
    pub(crate) async fn has_receipt_prefix(cards: &CardsHandle, prefix: &str) -> bool {
        cards.cards.lock().await.values().any(|c| {
            c.acc.timeline.iter().any(
                |item| matches!(&item.kind, state::TimelineKind::Receipt(text) if text.starts_with(prefix)),
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::App;
    use crate::bridge::test_support::{
        MockBackend, RecordingPlatform, build_app, realistic_parts, seed_entry, test_config, test_work_dir,
        turn_anchor,
    };

    fn ctx(session_id: &str, text: &str) -> PromptContext {
        PromptContext {
            session_id: session_id.into(),
            thread_key: ThreadKey::new("chat_1".into(), "chat_1".into()),
            text: text.into(),
            message_id: "msg_1".into(),
            subtitle: "p2p".into(),
            is_retry: false,
            requester_open_id: None,
            is_group: false,
            cola_message_id: None,
            images: Vec::new(),
            advisory_live: false,
        }
    }

    /// A takeover's seed lands only in the successor it was read for (spec
    /// #561, ticket #565): the transcript read runs unlocked, so a card
    /// session another Turn replaced meanwhile must never receive the
    /// orphan's content.
    #[test]
    fn the_orphan_seed_never_lands_on_a_replaced_successor() {
        let cursor = RenderedCursor {
            frontier: None,
            live_calls: ["call_sleep".to_string()].into_iter().collect(),
        };
        let seed = state::CursorSeed {
            frontier: None,
            live_calls: cursor.live_calls.clone(),
            resolved_live_calls: cursor.live_calls.clone(),
            scope: None,
        };
        let mut live = std::collections::HashMap::new();
        live.insert(
            "ses_test".to_string(),
            state::CardSession::new(
                state::StreamAccumulator::new("test"),
                Some("om_successor".to_string()),
            ),
        );

        // The successor this seed was read for: it lands.
        assert!(
            Turn::apply_orphan_seed(&mut live, "ses_test", "om_successor", &cursor, seed.clone()),
            "the read's own successor is seeded"
        );
        assert!(live["ses_test"].acc.seeded_calls.contains("call_sleep"));

        // Another Turn replaced the card session during the read: nothing is
        // seeded into the newer accumulator.
        live.insert(
            "ses_test".to_string(),
            state::CardSession::new(
                state::StreamAccumulator::new("test"),
                Some("om_newer".to_string()),
            ),
        );
        assert!(
            !Turn::apply_orphan_seed(&mut live, "ses_test", "om_successor", &cursor, seed),
            "a replaced successor receives nothing"
        );
        assert!(live["ses_test"].acc.seeded_calls.is_empty());
        assert!(live["ses_test"].acc.tools.is_empty());
    }

    /// The disposition-driven notice walks the table's classification: a
    /// Waiting disposition (a caller that forgot to guard) stays silent, while
    /// a Done disposition notifies with the true end's copy.
    #[tokio::test]
    async fn the_completion_notice_declines_a_disposition_that_has_not_ended() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
        Turn::seed_card(&app.cards_handle(), "ses_test", Some("om_1")).await;
        Turn::set_reply_target(&app.cards_handle(), "ses_test", "msg_1").await;
        Turn::set_turn_identity(
            &app.cards_handle(),
            "ses_test",
            crate::bridge::test_support::TEST_HOST,
            true,
            1,
        )
        .await;

        announce_completion(
            &app.cards_handle(),
            &app.feishu,
            &app.turn_config().notice_rules(),
            "ses_test",
            std::time::Instant::now(),
            &Disposition::Waiting,
            Some("om_1"),
        )
        .await;
        assert!(
            !platform.calls.lock().await.iter().any(|call| matches!(
                call,
                crate::bridge::test_support::PlatformCall::CompletionNotice { .. }
            )),
            "a waiting disposition must never be announced"
        );

        announce_completion(
            &app.cards_handle(),
            &app.feishu,
            &app.turn_config().notice_rules(),
            "ses_test",
            std::time::Instant::now(),
            &Disposition::Done,
            Some("om_1"),
        )
        .await;
        assert!(
            platform.calls.lock().await.iter().any(|call| matches!(
                call,
                crate::bridge::test_support::PlatformCall::CompletionNotice {
                    text,
                    ..
                } if text == "✅ 已完成。"
            )),
            "a Done disposition notifies with the true end's copy: {:?}",
            platform.calls.lock().await
        );
    }

    /// Finding (spec #602 pre-push review 5): `announce_completion` captured the
    /// card carrying the terminal write under one lock, then resolved the
    /// notice's recipient from a SEPARATE read of the session's current card. A
    /// newer Turn that replaces the card between those two reads redirects a
    /// DELIVERED notice to the replacement's requester.
    ///
    /// The map lock makes the race deterministic: the announce (spawned first)
    /// takes the lock, captures the ORIGINAL card and releases; the replacement
    /// (spawned second, already queued) swaps in a fully formed card before the
    /// announce resolves. The notice must still land on the ORIGINAL request's
    /// recipient, never the replacement's.
    #[tokio::test]
    async fn a_replace_between_captures_never_redirects_a_delivered_notice() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
        let cards = app.cards_handle();
        Turn::seed_card(&cards, "ses_test", Some("om_old")).await;
        Turn::set_reply_target(&cards, "ses_test", "msg_old").await;
        Turn::set_turn_identity(&cards, "ses_test", "ou_old", true, 1).await;

        // Hold the map lock so the announce and the replacement queue on it in
        // spawn order: the announce first, the newer Turn's replacement second.
        let held = cards.cards.lock().await;
        let (ready_tx, mut ready_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        let announce = {
            let cards = cards.clone();
            let platform = app.feishu.clone();
            let rules = app.turn_config().notice_rules();
            let ready = ready_tx.clone();
            tokio::spawn(async move {
                ready.send(()).expect("the test is receiving");
                announce_completion(
                    &cards,
                    &platform,
                    &rules,
                    "ses_test",
                    std::time::Instant::now(),
                    &Disposition::Done,
                    Some("om_old"),
                )
                .await;
            })
        };
        let replace = {
            let cards = cards.clone();
            let ready = ready_tx;
            tokio::spawn(async move {
                ready.send(()).expect("the test is receiving");
                // A newer Turn's own insert — ONE critical section installing
                // the replacement's card id, reply target and requester.
                let mut card = state::CardSession::new(
                    state::StreamAccumulator::new("turn"),
                    Some("om_new".to_string()),
                );
                card.acc.set_reply_target(Some("msg_new".to_string()));
                card.acc.set_requester(Some("ou_new_requester".to_string()));
                card.acc.set_is_group(true);
                cards.cards.lock().await.insert("ses_test".to_string(), card);
            })
        };
        ready_rx.recv().await.expect("the announce queued");
        ready_rx.recv().await.expect("the replacement queued");
        drop(held);
        announce.await.expect("the announce task");
        replace.await.expect("the replacement task");

        let notices = platform.completion_notices().await;
        assert!(
            notices
                .iter()
                .any(|(reply_to, open_id, _, _)| reply_to == "msg_old" && open_id == "ou_old"),
            "the request that ended is still notified: {notices:?}"
        );
        assert!(
            notices
                .iter()
                .all(|(reply_to, open_id, _, _)| reply_to != "msg_new" && open_id != "ou_new_requester"),
            "the replacement request is never announced the old Turn's ending: {notices:?}"
        );
    }

    /// Finding (spec #602 pre-push review 6): the ending was applied to a
    /// SPECIFIC card under one lock, but `announce_completion` then re-read the
    /// session's CURRENT card. A newer Turn that replaced the card between the
    /// stamp and the announce had its (default-`Delivered`) accumulator
    /// announced the OLD ending — to the NEW request's requester. The notice
    /// must be bound to the card the ending was applied to.
    #[tokio::test]
    async fn an_out_of_turn_ending_is_not_announced_on_a_replaced_card() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
        let cards = app.cards_handle();
        // The card the out-of-turn loop watches and stamps its ending on.
        Turn::seed_card(&cards, "ses_test", Some("om_old")).await;
        Turn::set_turn_anchor(&cards, "ses_test", &turn_anchor(1)).await;
        Turn::set_reply_target(&cards, "ses_test", "msg_old").await;
        Turn::set_turn_identity(&cards, "ses_test", "ou_old", true, 1).await;

        // The ending lands on om_old, atomically with the ownership re-check;
        // the returned id is the card it landed on.
        let stamped = ownership::Ticket::TurnAnchor(turn_anchor(1))
            .apply_ending_if_owned(&cards, "ses_test", &Disposition::Done)
            .await;
        assert_eq!(
            stamped.as_deref(),
            Some("om_old"),
            "the stamp names the card the ending landed on"
        );

        // The guard is released; a newer Turn replaces the card before the
        // notice resolves. Its accumulator's `ending_write` defaults to
        // `Delivered`, so the old ending must NOT ride it.
        let mut replacement =
            state::CardSession::new(state::StreamAccumulator::new("turn"), Some("om_new".to_string()));
        replacement.acc.set_reply_target(Some("msg_new".to_string()));
        replacement.acc.set_requester(Some("ou_new".to_string()));
        replacement.acc.set_is_group(true);
        cards
            .cards
            .lock()
            .await
            .insert("ses_test".to_string(), replacement);

        announce_completion(
            &cards,
            &app.feishu,
            &app.turn_config().notice_rules(),
            "ses_test",
            std::time::Instant::now(),
            &Disposition::Done,
            stamped.as_deref(),
        )
        .await;

        let notices = platform.completion_notices().await;
        assert!(
            notices.is_empty(),
            "a replaced card must never be announced the old ending: {notices:?}"
        );
    }

    /// The control for the replacement above: with no replacement, a delivered
    /// notice still targets the request that ended.
    #[tokio::test]
    async fn a_delivered_notice_uses_the_original_recipient() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
        let cards = app.cards_handle();
        Turn::seed_card(&cards, "ses_test", Some("om_old")).await;
        Turn::set_reply_target(&cards, "ses_test", "msg_old").await;
        Turn::set_turn_identity(&cards, "ses_test", "ou_old", true, 1).await;

        announce_completion(
            &cards,
            &app.feishu,
            &app.turn_config().notice_rules(),
            "ses_test",
            std::time::Instant::now(),
            &Disposition::Done,
            Some("om_old"),
        )
        .await;

        assert_eq!(
            platform.completion_notices().await,
            vec![(
                "msg_old".to_string(),
                "ou_old".to_string(),
                None,
                "✅ 已完成。".to_string()
            )],
            "a delivered notice names the request that ended"
        );
    }

    /// A failed Loading reply must not leave the session looking busy forever:
    /// the early error return used to skip the guard release.
    #[tokio::test]
    async fn failed_loading_reply_releases_the_guard() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut platform = RecordingPlatform::new();
        platform.fail_reply_card = true;
        let app = Arc::new(
            App::new(
                cfg,
                Arc::new(MockBackend::new(realistic_parts())),
                Arc::new(platform),
            )
            .unwrap(),
        );

        let err = Turn::run(&app.turn_handles(), ctx("ses_a", "hi")).await;

        assert!(err.is_err(), "the failed Loading reply must surface");
        assert!(
            !app.inflight.lock().await.contains("ses_a"),
            "the busy guard must be released"
        );
    }

    /// A prompt error still ends the turn and releases the guard, so the next
    /// message is not throttled.
    #[tokio::test]
    async fn prompt_error_releases_the_guard() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut backend = MockBackend::new(realistic_parts());
        backend.fail_prompt("provider 503");
        let (app, _platform) = build_app(cfg, backend).await;

        Turn::run(&app.turn_handles(), ctx("ses_a", "hi")).await.unwrap();

        assert!(!app.inflight.lock().await.contains("ses_a"));
    }

    /// The Turn Footer reports the variant the attempt actually sent, captured
    /// at send time (ADR-0019).
    #[tokio::test]
    async fn footer_records_the_variant_the_attempt_sent() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let backend = MockBackend::new(realistic_parts());
        let sent_variants = backend.prompt_variants.clone();
        let (app, _platform) = build_app(cfg, backend).await;
        let mut entry = crate::config::SessionEntry::new(
            ThreadKey::new("chat_1".into(), "chat_1".into()),
            "ses_a",
            "/tmp/a",
        );
        entry.variant = Some("high".into());
        seed_entry(&app, entry).await;

        Turn::run(&app.turn_handles(), ctx("ses_a", "hi")).await.unwrap();

        assert_eq!(
            sent_variants.lock().await.as_slice(),
            &[Some("high".to_string())],
            "the attempt sends the session's variant"
        );
        let cards = app.cards.lock().await;
        let footer_variant = cards
            .get("ses_a")
            .and_then(|c| c.acc.variant().map(str::to_string));
        assert_eq!(footer_variant.as_deref(), Some("high"));
    }

    /// A decoder that reports the model's variant (V2's message model ref
    /// carries it) is what the footer shows — the server's truthful selection
    /// even when cola's local mirror has none, and nothing is sent per prompt.
    #[tokio::test]
    async fn footer_reads_the_variant_the_transcript_reports() {
        use crate::backend::{MessageRole, ModelIdentity, SessionTranscript};

        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut backend = MockBackend::new(realistic_parts());
        let sent_variants = backend.prompt_variants.clone();
        let now = chrono::Utc::now().timestamp_millis();
        let user = crate::bridge::test_support::typed_message(
            "msg_u",
            MessageRole::User,
            Some(now),
            vec![crate::bridge::test_support::text_part("hi")],
        );
        let mut assistant = crate::bridge::test_support::typed_message(
            "msg_a",
            MessageRole::Assistant,
            Some(now + 1_000),
            realistic_parts(),
        );
        assistant.model = Some(ModelIdentity {
            provider_id: "opencode-go".into(),
            model_id: "deepseek-v4-flash".into(),
            variant: Some("high".into()),
        });
        backend.given_transcript("ses_a", vec![SessionTranscript::new(vec![user, assistant])]);
        let (app, _platform) = build_app(cfg, backend).await;
        seed_entry(
            &app,
            crate::config::SessionEntry::new(
                ThreadKey::new("chat_1".into(), "chat_1".into()),
                "ses_a",
                "/tmp/a",
            ),
        )
        .await;

        // The scripted history's user message is the turn's anchor: the drain
        // matches it by the cola-chosen id (ADR-0026), so the context must carry
        // the same id.
        let mut context = ctx("ses_a", "hi");
        context.cola_message_id = Some("msg_u".into());
        Turn::run(&app.turn_handles(), context).await.unwrap();

        assert_eq!(
            sent_variants.lock().await.as_slice(),
            &[None],
            "the durable generation sends no per-prompt variant"
        );
        let cards = app.cards.lock().await;
        let acc = cards.get("ses_a").expect("the turn's card");
        assert_eq!(
            acc.acc.variant(),
            Some("high"),
            "the footer reads the transcript's variant"
        );
        assert_eq!(acc.acc.model_id(), Some("deepseek-v4-flash"));
    }

    /// On a durable generation the turn's variant comes from the SESSION's
    /// selection, never a stale `/think` mirror: the mirror's `high` must not
    /// be sent (or tagged) when the session's model ref carries none.
    #[tokio::test]
    async fn turn_variant_ignores_the_mirror_on_a_durable_generation() {
        use crate::opencode::types::{ModelInfo, SessionSelection};

        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut backend = MockBackend::new(realistic_parts());
        let sent_variants = backend.prompt_variants.clone();
        backend.with_session_selection(
            "ses_a",
            SessionSelection {
                model: Some(ModelInfo {
                    id: "m".into(),
                    provider_id: "p".into(),
                    variant: None,
                }),
                agent: None,
            },
        );
        let (app, _platform) = build_app(cfg, backend).await;
        let mut entry = crate::config::SessionEntry::new(
            ThreadKey::new("chat_1".into(), "chat_1".into()),
            "ses_a",
            "/tmp/a",
        );
        entry.variant = Some("high".into());
        seed_entry(&app, entry).await;

        Turn::run(&app.turn_handles(), ctx("ses_a", "hi")).await.unwrap();

        assert_eq!(
            sent_variants.lock().await.as_slice(),
            &[None],
            "the stale mirror's variant must not ride the prompt"
        );
        let cards = app.cards.lock().await;
        assert!(
            cards
                .get("ses_a")
                .and_then(|c| c.acc.variant().map(str::to_string))
                .is_none(),
            "nor tag the footer"
        );
    }

    /// The retry matrix (spec #391) as a pure decision: a live run never gets a
    /// prompt; a settled turn takes a fresh id; an unpersisted or unfinished
    /// turn reuses the failed id; anything unknown takes a fresh id so the
    /// click always has an effect.
    #[test]
    fn retry_decision_reads_the_matrix() {
        use crate::backend::{FinishReason, MessageRole, Part, StepFinish};
        use crate::bridge::test_support::{text_part, typed_message};

        let window = |complete: bool| {
            let mut assistant = typed_message(
                "msg_a",
                MessageRole::Assistant,
                Some(2_000),
                vec![text_part("回答")],
            );
            if complete {
                assistant.parts.push(Part::StepFinish(StepFinish {
                    reason: FinishReason::Stop,
                }));
            }
            SessionTranscript::new(vec![
                typed_message(
                    "msg_cola_x",
                    MessageRole::User,
                    Some(1_000),
                    vec![text_part("hi")],
                ),
                assistant,
            ])
        };
        let unfinished = window(false);
        let settled = window(true);
        let id = Some("msg_cola_x");
        // The server-contract capability: V1 continues an admitted unfinished
        // turn on a same-id re-post, V2's admission makes it a no-op.
        let v1 = true;
        let v2 = false;

        // A live run is never re-prompted: the click re-attaches instead
        // (ticket #393).
        for status in [SessionStatus::Busy, SessionStatus::Retry] {
            assert!(
                matches!(
                    retry_decision(Some(status), Some(&settled), id, v1),
                    RetryDecision::Busy
                ),
                "{status:?} must not submit"
            );
        }
        // Unknown (status read failed/timed out, unreadable kind) → fresh id.
        assert!(matches!(
            retry_decision(None, Some(&settled), id, v1),
            RetryDecision::NewId
        ));
        assert!(matches!(retry_decision(None, None, id, v2), RetryDecision::NewId));
        // No id to reuse, or no transcript to read → fresh id.
        assert!(matches!(
            retry_decision(Some(SessionStatus::Idle), Some(&settled), None, v1),
            RetryDecision::NewId
        ));
        assert!(matches!(
            retry_decision(Some(SessionStatus::Idle), None, id, v2),
            RetryDecision::NewId
        ));
        // Settled → fresh id on BOTH generations (a same-id re-post against a
        // settled turn runs nothing on V2 and is not the honest history on V1).
        for reuse_continues in [v1, v2] {
            assert!(matches!(
                retry_decision(Some(SessionStatus::Idle), Some(&settled), id, reuse_continues),
                RetryDecision::NewId
            ));
        }
        // Admitted + unfinished: V1's upsert-continue → reuse; V2's admission
        // key would no-op the re-post → fresh id.
        assert!(matches!(
            retry_decision(Some(SessionStatus::Idle), Some(&unfinished), id, v1),
            RetryDecision::Reuse(reused) if reused == "msg_cola_x"
        ));
        assert!(matches!(
            retry_decision(Some(SessionStatus::Idle), Some(&unfinished), id, v2),
            RetryDecision::NewId
        ));
        // Nothing admitted (no anchor) → reuse on BOTH generations: both
        // create and run the never-admitted id.
        for reuse_continues in [v1, v2] {
            assert!(matches!(
                retry_decision(
                    Some(SessionStatus::Idle),
                    Some(&SessionTranscript::new(vec![])),
                    id,
                    reuse_continues
                ),
                RetryDecision::Reuse(reused) if reused == "msg_cola_x"
            ));
        }
    }
}
