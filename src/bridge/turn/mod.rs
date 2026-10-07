mod disposition;
mod flush;
mod follow;
mod ownership;
mod render;
mod settle;
mod state;

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
pub(crate) use ownership::{CardOwnership, StopDisposition};

/// The one ending vocabulary (spec #538): the table every path that ends a
/// card reads — the in-Turn paths through `StreamAccumulator::apply_ending`,
/// the out-of-turn loops through `Ticket::apply_ending_if_owned` (the
/// ownership check and the stamp under one lock), and the durable reap through
/// its `From<TurnSettle>` mapping. Crate-visible so a path outside the Turn
/// module (the reap's card translation in `chain::reconcile`) reads the same
/// table instead of a second state/failure mapping.
pub(crate) use disposition::Disposition;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tracing::Instrument;

use crate::backend::{MessageId, MessageRole, SessionTranscript, TurnAnchor, TurnSettle, WakeSource};
use crate::bridge::chain::{ChainRecord, RenderedCursor};
use crate::bridge::handler::image_inputs;
use crate::bridge::handles::{
    CardsHandle, FlowHandles, NoticeRules, RequestsHandle, SessionsHandle, TurnHandles,
};
use crate::bridge::span;
use crate::bridge::turn::state::StreamAccumulator;
use crate::config::ThreadKey;
use crate::feishu::client::ImageAttachment;
use crate::opencode;
use crate::opencode::types::SessionStatus;

/// How long one Backend read in the post-prompt drain may take before it is
/// abandoned. The drain's own bound caps this further per call: a hung
/// Backend must not hold the card (and the inflight guard) past the drain
/// deadline, and `/stop` must be observable within one bounded request.
const DRAIN_REQUEST_TIMEOUT_MS: u64 = 30_000;

/// How many consecutive drain reads must see no run before an unobserved
/// submit is treated as never having registered. A prompt admit only
/// *schedules* execution (ADR-0056), so a single non-busy read can precede the
/// run's registration; the value is the retired V2 poll fallback's confirmation
/// window, kept so the unstarted case is bounded by this window instead of the
/// whole drain budget. After it the turn finalizes from what it has — a reply
/// arriving later is deliberately not followed (the retired fallback's accepted
/// degradation). A failing or timing-out STATUS read takes the same window; a
/// failing TRANSCRIPT read is the drain's own rule (first-tick failure before
/// anything was observed ends the drain, later ones are retried).
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

/// The per-call timeout for one drain request: the fixed request bound,
/// shrunk to the remaining drain budget so a hung Backend cannot hold the
/// drain (or `/stop`) past its deadline. Never zero — a deadline already
/// passed still gets a token slice, enough for a healthy Backend to answer
/// and for a hung one to fail fast.
fn drain_request_timeout(deadline: tokio::time::Instant) -> u64 {
    let remaining = deadline
        .saturating_duration_since(tokio::time::Instant::now())
        .as_millis()
        .min(u128::from(DRAIN_REQUEST_TIMEOUT_MS)) as u64;
    remaining.max(1)
}

/// A fresh drain budget from the injected bound (`turn_drain_timeout_ms`):
/// every drain phase — the post-prompt drain and its single re-check — gets
/// its own, so a tiny injected bound keeps the whole lifecycle short and a
/// hung Backend can never fall back to the fixed request timeout.
fn drain_deadline(handles: &TurnHandles) -> tokio::time::Instant {
    tokio::time::Instant::now() + std::time::Duration::from_millis(handles.config.drain_timeout_ms())
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
        acc.reply_to_message_id = Some(message_id.clone());
        acc.session_id = Some(session_id.clone());
        // The notice's clock travels on the card (ADR-0060): a quiet true end
        // is settled by Session Sync, which has no Turn to read `started_at`
        // from.
        acc.turn_started_at = Some(started_at);
        // The id this turn's user message carries, so a later retry reuses
        // it (ADR-0026) — the server deduplicates by id.
        acc.cola_message_id = Some(cola_message_id.clone());
        // Full original prompt, so the error-card "retry" can re-submit it.
        acc.prompt = Some(text.clone());
        acc.requester_open_id = requester_open_id.clone();
        acc.is_group = is_group;
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
        acc.turn_generation = Some(generation);
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
                    card.acc.pending_gap = Some(gap);
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
                card.acc.session_id = Some(fresh_id.clone());
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
    /// a further message is treated as a Supplement) until that Turn is
    /// answered — then the card is marked Done and the guard released. A drain
    /// that runs out its bound while the session is still running is NOT
    /// completion (#284): the guard is released, but the card is handed to the
    /// out-of-turn [`follow`], which finalizes it when the session goes idle.
    async fn finish(&mut self, handles: &TurnHandles, prompt_resp: &crate::error::Result<()>) {
        // Post-prompt drain + the pre-finalization re-check: a Supplement
        // racing the drain's exit is drained here rather than dropped, and one
        // that lands after the release becomes a normal new Turn on the
        // handler's not-busy path.
        let drain_outcome = self.drain_after_prompt(handles).await;

        // The turn's outcome is OBSERVED, not returned (ADR-0056): a rejected
        // submit is the call's Err, and a submitted run's failure is recorded
        // on its newest assistant message — the blocking response used to carry
        // it inline, and the transcript now does. One read serves both the
        // error decision and the final reconcile below.
        //
        // A read that fails or has no anchor is NOT completion: the failure the
        // drain last observed stands in, so a hiccup cannot stamp Done over a
        // failed turn.
        let final_transcript = handles.backend.transcript(&self.session_id).await.ok();

        // A deliberate `/stop` owns this turn's ending (#394): the abort the
        // server recorded is NOT a failure, its text must never reach the
        // card, and the card finalizes `Stopped` — here, or by the follow's own
        // stop branch if the drain had already handed the card off. The marker
        // is sticky until the next Turn's `start` clears it. Read it AFTER the
        // transcript read: that read is an await, so a stop landing during it
        // must still win.
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
        // Read before the match consumes the decision (a `Failed` carries its
        // message): an undecided read keeps the follow observing.
        let final_running = final_settle == Some(TurnSettle::Running);
        // The ending the settle decision gives the Turn, through the one
        // disposition table: a decided read is its disposition; an undecided
        // one (a Wake's Execution has not reached its boundary yet, or the read
        // failed) keeps the drain's own last word — Waiting or Unreceived when
        // that found one, else no ending yet (Observe).
        let ending = match final_settle {
            Some(TurnSettle::Running) | None => match drain_outcome {
                Some(DrainState::Waiting) => Disposition::Waiting,
                Some(DrainState::Unreceived) => Disposition::Unreceived,
                _ => Disposition::Observe,
            },
            Some(settle) => Disposition::from(settle),
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
                // No ending yet (the undecided, unfollowed read), the true
                // end, and a decided `Failed` whose message did not surface
                // through `prompt_err` (the same projection: the final read
                // lost the anchor) all finalize Done, exactly as the replaced
                // else branch did.
                Disposition::Observe | Disposition::Done | Disposition::Failed(_) => Disposition::Done,
                // The loop-only endings are never settle outcomes. Naming them
                // keeps this mapping exhaustive, so a new disposition is a
                // compile error here, not a silent Done.
                Disposition::Stopped | Disposition::LostContact | Disposition::StuckPanel => {
                    Disposition::Done
                }
            },
        };

        // The card is followed instead of finalized when the drain bound was
        // reached with the session still running (#284) or the final read
        // itself is undecided (a Wake's Execution has not reached its
        // boundary): the follow keeps the SAME accumulator and card chain and
        // settles it when the session reports non-busy. The armed anchor is
        // the follow's identity; with none — the submitted message never
        // landed (ADR-0062) — the card goes to the unreceived watch instead,
        // which owns it by chain identity and captures the anchor the moment
        // the message appears. A stopped turn never hands off: the stop is its
        // ending, so it finalizes right here (#394).
        let follow =
            !stopped && prompt_err.is_none() && (drain_outcome == Some(DrainState::Running) || final_running);
        let follow_anchor = if follow {
            Turn::armed_turn_anchor(&handles.cards, &self.session_id).await
        } else {
            None
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
        // time, ADR-0019), and `self` is gone once the follow owns the card —
        // apply it before either end. The model/token halves are captured from
        // the transcript's messages themselves, so the follow's renders keep
        // them current.
        if let Some(card) = handles.cards.cards.lock().await.get_mut(&self.session_id) {
            card.acc.variant = self.turn_variant.clone();
        }

        // A followed card is NOT finalized here: the follow owns it now. Its
        // render ticks keep the footer's context window current, and its own
        // finalization refreshes the work context and flushes. Everything
        // below the guard is the normal end of a turn.
        if !follow {
            // Reconcile: render any parts the incremental poll missed from the
            // settled transcript read above, then apply this Turn's ending.
            //
            // A stop landing after the read above (during the leftover
            // rejection, or any scheduler hop since) must still win: re-read
            // the marker immediately before the stamp. The residual window is
            // the cards-lock acquisition below; a stop landing after it is
            // indistinguishable from one landing just after a completed turn,
            // and the next Turn owns it.
            stopped = stopped || handles.waits.is_stopped(&self.session_id).await;
            if stopped {
                // The stop is this Turn's ending, whatever the settle decision
                // said (#394): it discards a recorded failure and the card
                // takes its own terminal.
                disposition = Disposition::Stopped;
            }
            {
                let mut cards = handles.cards.cards.lock().await;
                if let Some(acc) = cards.get_mut(&self.session_id).map(|c| &mut c.acc) {
                    if let Some(transcript) = &final_transcript {
                        render::render_new_turn_parts(acc, transcript);
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
                                    acc.context_tokens = used;
                                }
                            }
                        }
                    }
                    // The one ending application every path shares (spec #538):
                    // the card's state, failure line and phase timer all come
                    // from the disposition decided above — a stop discards a
                    // recorded failure, a failure records its own line, and the
                    // yield / true end leave it as it was.
                    acc.apply_ending(&disposition);
                    tracing::info!(
                        "final render: fetched_messages={} text={} reasoning={} tools={} rendered_parts={} error={}",
                        final_transcript
                            .as_ref()
                            .map(|transcript| transcript.messages.len())
                            .unwrap_or(0),
                        acc.text.len(),
                        acc.reasoning.len(),
                        acc.tools.len(),
                        acc.rendered_parts.len(),
                        acc.error.as_deref().unwrap_or("none"),
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
        }

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
        // notify them. A followed turn notifies when the FOLLOW finalizes (its
        // real end), not at the drain bound; the disposition's own
        // classification declines a Waiting yield, so it never notifies — the
        // notice belongs to the true end (ADR-0059).
        if !follow {
            send_completion_notice(
                &handles.cards,
                &handles.platform,
                &handles.config.notice_rules(),
                &self.session_id,
                self.started_at,
                &disposition,
            )
            .await;
        }

        // The guard is released here for every end except a follow hand-off:
        // the follow (the unreceived watch included) inherits it (ADR-0059,
        // ADR-0062) and hands it back when its own loop ends, so the follow
        // window has no guard-free gap and `busy()` (the server-yield read)
        // never sees a followed Session as idle.
        if !follow {
            self.release(handles).await;
        }
        if follow {
            tracing::info!(
                "turn drain: session {} not finalized; handing off to the follow",
                self.session_id
            );
            // No release happened above: the follow inherits the guard this
            // Turn is still holding (ADR-0059) and hands it back when its loop
            // ends. `anchor` is None when the submitted message never landed:
            // the follow then watches the card chain until it does — or ends
            // it Unreceived (ADR-0062).
            follow::spawn(
                handles,
                follow::FollowFacts {
                    session_id: self.session_id.clone(),
                    thread_key: self.thread_key.clone(),
                    directory: self.directory.clone(),
                    started_at: self.started_at,
                    anchor: follow_anchor,
                },
            )
            .await;
        }
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
    /// BEFORE the card is marked Done and the guard released, no matter how
    /// the drain ended (settled, the bound, or a failed read). The re-check
    /// gets its own fresh budget from the same injected bound, so it can never
    /// hold the card (or delay `/stop`) past a tiny injected drain timeout. A
    /// Supplement the re-check finds is drained again — one bounded drain, so
    /// a session that merely stays busy cannot extend finalization. A
    /// Supplement that lands after the release is seen by the handler's
    /// not-busy path and becomes a normal new Turn.
    ///
    /// Returns the state the last drain observed at its bound when that was
    /// still pending (`Some(Running)` / `Some(Supplement)`), `Some(Waiting)`
    /// when the settle decision ended it with live Background Tasks,
    /// `Some(Unreceived)` when the decision found the submitted message never
    /// landed at an idle session, `None` when the drain settled or never saw
    /// anything pending. `finish` turns bound-with-`Running` into the
    /// out-of-turn follow (#284) instead of finalizing a card under a live
    /// session, and `Some(Waiting)` / `Some(Unreceived)` into the card's own
    /// ending (ADR-0059, ADR-0062).
    async fn drain_after_prompt(&mut self, handles: &TurnHandles) -> Option<DrainState> {
        let last = self.drain(handles).await;
        // A waiting yield or an Unreceived ending is the decision: no
        // racing-Supplement re-check may reopen the drain (its render would add
        // content to a card that is about to stop updating, and the ending was
        // already decided).
        if matches!(last, Some(DrainState::Waiting | DrainState::Unreceived)) {
            return last;
        }
        if self
            .drain_tick(handles, drain_request_timeout(drain_deadline(handles)))
            .await
            == Some(DrainState::Supplement)
        {
            tracing::info!(
                "turn drain: supplement racing the finish on session {}; draining",
                self.session_id
            );
            return self.drain(handles).await;
        }
        last
    }

    /// Poll the Backend, render it into the live card, and stop when nothing
    /// is pending or the bound is reached. A failed Backend read while a run
    /// was already observed pending is retried (the render poll's own policy):
    /// dropping the drain on one transient error would lose the reply this
    /// phase exists to render. Each request is bounded by the remaining drain
    /// budget, so a hung Backend cannot hold the drain past the bound.
    ///
    /// `Some(state)` means the bound was reached with `state` the last thing
    /// observed — the caller must not read it as completion (#284) — except
    /// for [`DrainState::Waiting`] and [`DrainState::Unreceived`], the settle
    /// decision's endings, which end the drain immediately; `None` means the
    /// drain settled, stopped, or never observed anything pending.
    async fn drain(&mut self, handles: &TurnHandles) -> Option<DrainState> {
        let poll_ms = handles.config.render_poll_ms();
        let deadline = drain_deadline(handles);
        let mut last: Option<DrainState> = None;
        loop {
            // The first check runs before any sleep: a Supplement that missed
            // the run is already on the Backend when the submit returns.
            match self.drain_tick(handles, drain_request_timeout(deadline)).await {
                Some(DrainState::Settled) => return None,
                // The Turn yields waiting (ADR-0059) or ends Unreceived
                // (ADR-0062): the ending is decided, so there is nothing left
                // to observe — the card stops updating (the next Wake
                // continues a waiting chain on a new card).
                Some(state @ (DrainState::Waiting | DrainState::Unreceived)) => return Some(state),
                Some(state) => last = Some(state),
                None if last.is_none() => return None,
                None => {}
            }
            if tokio::time::Instant::now() >= deadline {
                tracing::info!("turn drain: bound reached on session {}", self.session_id);
                return last;
            }
            tokio::time::sleep(std::time::Duration::from_millis(poll_ms)).await;
        }
    }

    /// One drain tick: read the Backend, decide whether rendering must go on,
    /// and — when it must — render the very snapshot the decision was made
    /// from, so the live card follows the new Turn. `None` means the Backend
    /// read failed (unknown state).
    async fn drain_tick(&mut self, handles: &TurnHandles, timeout_ms: u64) -> Option<DrainState> {
        let transcript = self.drain_transcript(handles, timeout_ms).await?;
        match self.drain_state(handles, &transcript, timeout_ms).await {
            DrainState::Settled => Some(DrainState::Settled),
            pending => {
                render::render_and_flush(
                    &handles.cards,
                    &handles.sessions,
                    &handles.backend,
                    &handles.requests,
                    &self.session_id,
                    &transcript,
                )
                .await;
                Some(pending)
            }
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
    ) -> DrainState {
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
            return DrainState::Settled;
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
                return DrainState::Supplement;
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
                DrainState::Running
            }
            // A non-busy status: the settle decision owns the ending (ADR-0059,
            // ADR-0062) once the run was observed; before that the
            // confirmation window still bounds an unregistered submit. The
            // failed and timed-out paths pass [`IdleRead::Unreadable`]: no
            // evidence the session is not live, so it can never decide the
            // Unreceived ending — see `settle_or_yield`.
            Some(Ok(_)) => self.settle_or_yield(transcript, anchor.as_ref(), observed, IdleRead::Idle),
            Some(Err(e)) => {
                tracing::warn!("turn drain session status: {}", e);
                self.settle_or_yield(transcript, anchor.as_ref(), observed, IdleRead::Unreadable)
            }
            // The bounded status call timed out: same rule as a failed read.
            None => self.settle_or_yield(transcript, anchor.as_ref(), observed, IdleRead::Unreadable),
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

    /// The drain's Backend read, bounded by the caller's per-call timeout so a
    /// hung Backend cannot hold the card (and the inflight guard) past the
    /// drain bound.
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
        }
    }

    /// Whether this cause continues a card that has already ENDED: a Wake
    /// arrives after the Turn yielded 等待后台任务 or its card ended, so the
    /// handoff must not restamp a terminal card's ending, and the continuation
    /// starts a fresh live phase (the ended attempt's display facts stay
    /// behind). Every other cause splits a card that is still live.
    pub(crate) fn continues_an_ended_card(self) -> bool {
        matches!(self, Self::Wake)
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
    /// recorded turn start).
    Settled {
        disposition: Disposition,
        notice_at: Option<std::time::Instant>,
    },
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
    pub(crate) async fn flush_card(cards: &CardsHandle, session_id: &str) {
        let write_lock = cards.write_lock(session_id).await;
        let _guard = write_lock.lock().await;
        flush::flush_card_locked(cards, session_id, flush::SplitPolicy::Allow).await;
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
                    card.acc.cola_message_id.clone().map(MessageId::new).or_else(|| {
                        card.acc
                            .turn_anchor
                            .as_ref()
                            .map(|anchor| anchor.message_id.clone())
                    }),
                    card.acc.turn_anchor.as_ref().map(|anchor| anchor.created_ms),
                    card.acc.directory.clone(),
                    // The chain's durable reply target (issue #580): the Feishu
                    // message the card replies under — an OpenCode message id
                    // is never a deliverable target.
                    card.acc.reply_to_message_id.clone(),
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
            if let Some(card) = live.get_mut(session_id)
                && card.acc.cursor == RenderedCursor::default()
            {
                card.acc.cursor = cursor;
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
                card.acc.cola_message_id.clone().map(MessageId::new).or_else(|| {
                    card.acc
                        .turn_anchor
                        .as_ref()
                        .map(|anchor| anchor.message_id.clone())
                }),
                card.acc.turn_anchor.as_ref().map(|anchor| anchor.created_ms),
                card.acc.directory.clone(),
                // The chain's durable reply target (issue #580): a split whose
                // supplement moved the anchor carries the newest one.
                card.acc.reply_to_message_id.clone(),
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
            if let Some(card) = live.get_mut(session_id)
                && card.acc.cursor == RenderedCursor::default()
            {
                card.acc.cursor = cursor;
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
                card.acc.pending_gap = Some(gap);
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
            .and_then(|card| card.acc.cola_message_id.clone())
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
            card.acc.pending_gap = Some(gap);
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
        if card.acc.receive_hint_shown {
            return false;
        }
        card.acc.receive_hint_shown = true;
        card.acc.push_receipt(RECEIVE_HINT);
        true
    }

    /// Hand an ENDED card chain over to a Wake continuation (ADR-0059): write
    /// the ledger handover the continuation owes the chain it is leaving
    /// (ADR-0060), queue the [`SplitKind::Wake`] split and flush it, so the
    /// tracked card is finalized and a NEW continuation card — replied to
    /// `reply_to`, opening with the 承接 receipt — becomes the chain's newest
    /// card. A Waiting card is stamped 「部分完成，继续中…」 (its wait is over,
    /// the chain moves on); a terminal card keeps the ending it recorded —
    /// unless the handover just wrote it, in which case even a terminal card is
    /// PATCHed, with its ending intact. Unlike [`Self::split_card_chain`], the
    /// chain must NOT be owned by a live Turn/renderer: the Wake decision reads
    /// a snapshot, and a Wake continuation must never split a card somebody
    /// else is still streaming into. `line` carries the 承接 line's key (just
    /// before the resumed work, whose server times are already in the past at
    /// poll time) and the Wake it covers.
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
    /// Returns false — nothing queued — when the session has no card or a live
    /// renderer owns it.
    pub(crate) async fn split_chain_for_wake(
        cards: &CardsHandle,
        session_id: &str,
        reply_to: &str,
        line: ContinuationLine,
        transcript: &SessionTranscript,
        now_ms: i64,
    ) -> bool {
        let write_lock = cards.write_lock(session_id).await;
        let _guard = write_lock.lock().await;
        {
            let mut live = cards.cards.lock().await;
            let Some(card) = live.get_mut(session_id) else {
                return false;
            };
            if card.acc.card_state.is_render_owned() {
                return false;
            }
            let anchor = card.acc.turn_anchor.clone();
            let handover = anchor.as_ref().is_some_and(|anchor| {
                render::apply_ledger_read(
                    &mut card.acc,
                    transcript,
                    &HashMap::new(),
                    Some(anchor),
                    now_ms,
                    state::LedgerCadence::Minute,
                )
            });
            card.pending_split.push(state::PendingSplit {
                reply_to: reply_to.to_string(),
                kind: SplitKind::Wake,
                receipt_pushed: false,
                line: Some(line),
                handover,
            });
        }
        flush::flush_card_locked(cards, session_id, flush::SplitPolicy::Allow).await;
        true
    }

    /// Resume a yielded card IN PLACE for a shell/subagent completion Wake
    /// (ADR-0066): the one-card-per-request handoff, beside the split. One
    /// write-lock-held sequence, exactly like the split handover: the read's
    /// remaining live list and the retiring Wake's fixed completion entry are
    /// written onto the card's OWN accumulator, so the entry lands at its own
    /// moment on the card that hosted the task (ADR-0060) and the live list
    /// stays; the card then takes [`CardState::Resuming`] and the flush
    /// renders the whole live slice on the SAME card. No card is sent and
    /// nothing is replied to: the PATCH is the announcement, so it drains the
    /// staged Wake Watermark exactly as the 承接 line's send did (ADR-0061) —
    /// a restart after it cannot re-post the Wake.
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
    /// never resume a card somebody else took over. `wake_id` is the completion
    /// this resume takes over, marked as such in the same locked write, so a
    /// later tail past it splits (ADR-0059) and the resume is never re-decided.
    /// `now_ms` is the read's clock — the Session Sync pass's own, shared with
    /// the split handover of the same read.
    pub(crate) async fn resume_yielded_card(
        cards: &CardsHandle,
        session_id: &str,
        wake_id: &str,
        transcript: &SessionTranscript,
        now_ms: i64,
    ) -> bool {
        let write_lock = cards.write_lock(session_id).await;
        let _guard = write_lock.lock().await;
        {
            // The yielded-card write admission, re-checked under the write lock
            // through the module's lock-scoped helper: the returned card is
            // still locked, so this admission and the write below share one
            // lock and no collect, split or ending can slip between them.
            let Some(mut card) = CardOwnership::admit_ledger_write(cards, session_id).await else {
                return false;
            };
            let anchor = card.acc.turn_anchor.clone();
            render::apply_ledger_read(
                &mut card.acc,
                transcript,
                &HashMap::new(),
                anchor.as_ref(),
                now_ms,
                state::LedgerCadence::Second,
            );
            // The chain has taken this completion's work over. Its entry may
            // already be on the card (a yielded ledger refresh placed it while
            // the work was unrendered): the announcement set stays untouched,
            // so the entry is never doubled — only the handoff is recorded.
            card.acc.hand_over_wake(wake_id);
            card.acc.set_resuming();
        }
        flush::flush_card_locked(cards, session_id, flush::SplitPolicy::Allow).await;
        true
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
        // re-checks the same rule under the card-write lock.
        let admitted = {
            let live = cards.cards.lock().await;
            live.get(session_id).is_some_and(ownership::admits_ledger_refresh)
        };
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
        let (changed, settled, notice_at) = {
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
                anchor.as_ref(),
                now_ms,
                state::LedgerCadence::Second,
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
            let notice_at = if card.acc.wake_continuation {
                None
            } else {
                card.acc.turn_started_at
            };
            (changed, settled, notice_at)
        };
        if let Some(disposition) = settled {
            // The footer is refreshed one last time (ADR-0019): the card
            // stopped updating at the yield, and the work behind it may have
            // moved the branch or the tree. Under the same write lock as the
            // flush, so the refreshed footer and the ending ride one PATCH.
            // Best effort, like every other finalize. The flush's own entry
            // drops the card's now-spent durable record (ADR-0063).
            state::refresh_work_context(cards, session_id).await;
            flush::flush_card_locked(cards, session_id, flush::SplitPolicy::Forbid).await;
            tracing::info!("yielded card settled in place: session {session_id} ({disposition:?})");
            return YieldedUpdate::Settled {
                disposition,
                notice_at,
            };
        }
        if !changed {
            return YieldedUpdate::Unchanged;
        }
        // The guard (ADR-0060): a ledger-only refresh must never post a new
        // card, so this flush may not finalize and continue the chain.
        flush::flush_card_locked(cards, session_id, flush::SplitPolicy::Forbid).await;
        YieldedUpdate::Refreshed
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
    /// `None` when the session's accumulator vanished (the caller should stop).
    pub(crate) async fn render_and_flush(
        cards: &CardsHandle,
        sessions: &SessionsHandle,
        backend: &Arc<dyn crate::backend::Backend>,
        requests: &RequestsHandle,
        session_id: &str,
        transcript: &SessionTranscript,
    ) -> Option<render::RenderStats> {
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
            .and_then(|c| c.acc.reply_to_message_id.clone())
    }

    /// Seed `acc`'s Wake floor from the chain's durable Wake Watermark (spec
    /// #561, review #569; ADR-0061): every Wake at or below the mark was
    /// announced by an earlier card — the mark outlives the record it was
    /// announced on — so this card's render neither re-inserts its completion
    /// entry nor stages a watermark advance for it. A strictly newer Wake still
    /// renders and announces.
    fn seed_wake_floor(cards: &CardsHandle, session_id: &str, acc: &mut state::StreamAccumulator) {
        acc.wake_floor = cards.chains.announced(session_id).map(|mark| mark.created_ms);
    }

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
            .map(|c| c.acc.progress_mark)
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
    async fn apply_disposition_if_anchor(
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
        if card.acc.card_state != from
            || card.acc.recovery_claimed
            // A Wake continuation carries no question to re-ask (ADR-0059):
            // its card never offers Retry, so no click may claim one either.
            || card.acc.wake_continuation
        {
            return None;
        }
        // A card that was never sent cannot be marked; an Error card always
        // has one (`Turn::start` drops the session when the Loading reply
        // fails).
        card.card_message_id.as_ref()?;
        // An external turn's ending has no prompt to re-submit; the button
        // must not claim (there is nothing to recover).
        let prompt = card.acc.prompt.clone().filter(|prompt| !prompt.is_empty())?;
        card.acc.recovery_claimed = true;
        Some(TurnRecovery {
            session_id: session_id.to_string(),
            prompt,
            reply_to: card.acc.reply_to_message_id.clone().unwrap_or_default(),
            subtitle: card.acc.title.clone(),
            requester_open_id: card.acc.requester_open_id.clone(),
            is_group: card.acc.is_group,
            cola_message_id: card.acc.cola_message_id.clone(),
        })
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
    /// first sleep, then the out-of-turn [`follow`] is spawned: it inherits the
    /// Session's guard — this path held none — exactly like the drain hand-off
    /// (ADR-0059), and its window owns Busy→non-busy finalization, `/stop`, the
    /// graces and the silent exit when a new Turn replaces the accumulator.
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
            card.acc.card_state = crate::feishu::card::CardState::Retried;
        }
        Self::flush_card(cards, session_id).await;
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
        let mut live = cards.cards.lock().await;
        if live.contains_key(session_id) {
            return None;
        }
        let mut acc = StreamAccumulator::new(title);
        Self::seed_wake_floor(cards, session_id, &mut acc);
        acc.turn_anchor = Some(anchor.clone());
        acc.session_id = Some(session_id.to_string());
        // The chain's reply target (issue #580) is the target the successor's
        // create actually lands on — the durable reply target, or the recorded
        // card a refused target falls back to. `take_over_armed_card` sets it
        // under the takeover's own critical section; an OpenCode message id is
        // never a deliverable Feishu target, so the anchor is not one.
        acc.variant = variant;
        // The successor continues a chain: it carries no question to re-ask,
        // so an Error ending never offers Retry (ADR-0059).
        acc.wake_continuation = true;
        acc.apply_work_context(work_context);
        acc.seed_projection(cursor, seed.clone());
        // A pending orphan gap renders first (spec #561, review #569): its
        // content was never on a card, while the chain's cursor has already
        // advanced past the delivered content that follows it. The seed then
        // governs the successor's own window — the delivered content is never
        // re-rendered.
        if let Some(gap) = gap {
            acc.pending_gap = Some(gap.clone());
        }
        let rendered = render::render_turn_parts(&mut acc, transcript);
        render::apply_ledger_read(
            &mut acc,
            transcript,
            &std::collections::HashMap::new(),
            Some(anchor),
            chrono::Utc::now().timestamp_millis(),
            state::LedgerCadence::Minute,
        );
        match ending {
            Some(ending) => acc.apply_ending(ending),
            None => {
                // A live adoption follows a run already in flight: the
                // successor is a working card from its first send, never an
                // initial 「思考中」.
                if acc.card_state == crate::feishu::card::CardState::Loading {
                    acc.card_state = crate::feishu::card::CardState::Streaming;
                }
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
        acc.session_id = Some(session_id.to_string());
        acc.reply_to_message_id = Some(card_id.to_string());
        acc.attach_work_context(session_dir).await;
        acc.variant = variant;
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
}

/// What Session Sync owes for one read (ADR-0059): the decision carries
/// everything the caller needs to act on it, so the same lock acquisition
/// that found the chain also says which handoff to use — nothing is
/// re-derived under a second look.
pub(crate) enum WakeContinuation {
    /// A card chain exists whose newest placeable Wake is a shell/subagent
    /// completion this chain has not yet TAKEN OVER, and whose card is still
    /// yielded 「⏳ 等待后台任务」: resume that card IN PLACE (ADR-0066) — no new
    /// card, no 承接 line. The retiring task's completion entry and the
    /// remaining live list land on the card the task lived on, the card takes
    /// the resuming state, and the shared out-of-turn settle loop streams the
    /// resumed work into it. `wake_id` is the completion this resume takes
    /// over, marked as such under the delivery's own write lock — so a later
    /// tail past it splits (ADR-0059), while the entry a yielded ledger refresh
    /// already placed stays single. Decided only for the state this delivery
    /// admits ([`ownership::admits_ledger_refresh`]), so a decision and its
    /// write can never disagree about which card resumes.
    ResumeInPlace { wake_id: String },
    /// A card chain exists: continue it by split. Only the content the chain
    /// has not rendered lands on the continuation, and the accumulator's own
    /// anchor scopes the settle decision. `line` is the new card's opening
    /// 承接 line, whose key sits just before the work this continuation will
    /// render (that work is already in the past at poll time, so a key at
    /// cola's "now" would sort the receipt after it — the live order bug).
    ContinueChain { line: ContinuationLine },
    /// No chain in this process and no durable record (a cola restart that
    /// left nothing behind): arm a fresh card, scoped at the newest Wake's own
    /// anchor so the lost card's content is never replayed.
    Fresh { anchor: TurnAnchor },
}

/// The opening 承接 line of a Wake continuation card (ADR-0059): the timeline
/// key the line takes — just before the work the continuation renders — and
/// the Wake whose completion the line already announces, with its own server
/// time (the durable Wake Watermark's value once the line's card sends,
/// ADR-0061), so the merged-path receipt (the render pass's) cannot double it.
/// `wake` is `None` for the content-diff fallback, which answers no Wake.
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
    ///   re-posted on every poll. What it owes is then one of two handoffs: a
    ///   yielded card whose newest placeable Wake is a shell/subagent
    ///   completion the chain has not taken over yet resumes in place
    ///   ([`WakeContinuation::ResumeInPlace`], ADR-0066), and every other
    ///   continuation continues the chain by split.
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
                // A yielded card resumes IN PLACE (ADR-0066) iff the newest
                // placeable Wake — the completion whose work this continuation
                // would render — is a shell/subagent completion this chain has
                // not TAKEN OVER yet. One card per request: the entry, the
                // resumed work and the ending all stay on the card the user's
                // message opened. The gate reads the HANDOFF, not the
                // announcement: a yielded card's ledger refresh places the
                // completion entry while the Wake's work is still unrendered
                // (live 2026-10-01 — the resumed message's text part was empty
                // at that read), and that entry must not masquerade as a
                // handoff, or the work would split into a 承接 card when it
                // arrives. Every other continuation keeps the ADR-0059 split: a
                // restart/interrupt Wake is not a task completion (no entry to
                // place), a Wake this chain already handed over (a 承接 line or
                // an earlier resume) is the Wake-less content-diff fallback — a
                // later tail past it must not re-open the card — and a card past
                // its wait (terminal, or a handoff already owed) must not be
                // re-opened either: a ✅ flipping back to 🔄 would misread the
                // ending it recorded.
                if ownership::admits_ledger_refresh(card)
                    && let Some(wake) = newest_wake
                    && matches!(wake.source, WakeSource::Shell | WakeSource::Subagent)
                    && !card.acc.handed_over_wakes.contains(wake.id.as_str())
                {
                    return Some(WakeContinuation::ResumeInPlace {
                        wake_id: wake.id.to_string(),
                    });
                }
                // Key the 承接 receipt just before the work the continuation
                // will render: the newest Wake's own server time when there is
                // one, else the Turn's own content (the content-diff
                // fallback). That work's parts carry server times already in
                // the past at poll time, so they sort after this key. The
                // covered Wake is marked too: its completion is announced by
                // this line, never doubled by the merged-path entry.
                let wake = newest_wake.and_then(|wake| wake.anchor());
                let at = wake
                    .as_ref()
                    .map_or(turn_anchor.created_ms, |wake| wake.created_ms)
                    .saturating_sub(1);
                return Some(WakeContinuation::ContinueChain {
                    line: ContinuationLine {
                        at,
                        wake: wake.map(|wake| (wake.message_id.to_string(), wake.created_ms)),
                    },
                });
            }
        }
        // No chain in this process (a cola restart). The Fresh gate is the
        // Chain Record module's second decision entry (ADR-0069): it shares
        // this store's durable facts — the Chain Record itself and the Wake
        // Watermark — and the Keep / NoDecision vocabulary, and owns the rules
        // the read facts are judged by. The corroboration the gate cannot
        // decide — the in-process inbound claim and the anchor re-read — stays
        // with `render_wake_continuation`'s caller.
        let wake = newest_wake.and_then(|wake| wake.anchor());
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
            .and_then(|card| card.acc.cola_message_id.clone())
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
        acc.session_id = Some(session_id.to_string());
        acc.reply_to_message_id = facts.reply_to.map(str::to_string);
        acc.variant = facts.variant;
        acc.wake_continuation = true;
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
    /// reached and applied, or `None` when it stopped owning the card (its
    /// accumulator vanished or a successor took it over) and stamped nothing;
    /// the caller owns the announcement. A split continuation needs none — its
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
    ) -> Option<Disposition> {
        let ticket = ownership::Ticket::Chain {
            chain,
            anchor: anchor.clone(),
        };
        let disposition = settle::run(flow, session_id, directory, timing, &ticket).await?;
        // The same post-run gap as the follow's: the loop's last probe may be
        // stale by now, so the ending is applied atomically with a fresh
        // ownership check. Ownership lost is a loop that ended nothing — the
        // same `None` `run` returns.
        if !ticket
            .apply_ending_if_owned(&flow.cards, session_id, &disposition)
            .await
        {
            return None;
        }
        Some(disposition)
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
            .and_then(|card| card.acc.turn_started_at)
    }

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
    /// `cursor_stage` is the exact Rendered Cursor stage the create's body
    /// carried: it is confirmed in the SAME critical section as the re-point
    /// (review #569), so the record never names the successor with the
    /// predecessor's frontier. [`ArmedTakeover::Lost`] means the session was
    /// replaced or attached meanwhile: nothing was touched. Otherwise the
    /// successor id is attached, the record re-pointed carrying the confirmed
    /// cursor into the armed accumulator exactly as [`Self::track_live_card`]
    /// does, and the predecessor record handed back for the caller's deferred
    /// collect.
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
    /// continues (no predecessor collect). Returns `false` when a fresh Turn
    /// owns the session now: the caller collects the late card and stops the
    /// chain.
    pub(crate) async fn track_projected_continuation(
        cards: &CardsHandle,
        session_id: &str,
        chain_id: u64,
        card_message_id: &str,
        card_is_live: bool,
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
            // The armed takeover already set the chain's reply target; a
            // continuation keeps it (issue #580).
            None,
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
        // The armed successor's landing reply target (issue #580): the target
        // its create actually reached — the durable reply target, or the
        // recorded card a refused target fell back to. Set before the record
        // facts are read so the re-point and the accumulator never disagree.
        if let Some(reply_to) = reply_to {
            card.acc.reply_to_message_id = Some(reply_to.to_string());
        }
        let (message_id, created_ms, context_directory, reply_to) = (
            card.acc.cola_message_id.clone().map(MessageId::new).or_else(|| {
                card.acc
                    .turn_anchor
                    .as_ref()
                    .map(|anchor| anchor.message_id.clone())
            }),
            card.acc.turn_anchor.as_ref().map(|anchor| anchor.created_ms),
            card.acc.directory.clone(),
            card.acc.reply_to_message_id.clone(),
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
            // predecessor cursor) — not even between two chains operations.
            Some((cursor, gap)) => cards.chains.track_carrying_cursor(
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
            // as a plain re-point does.
            None => cards.chains.track(
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
            if card.acc.cursor == RenderedCursor::default() {
                card.acc.cursor = cursor;
            }
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
async fn release_inflight(handles: &TurnHandles, session_id: &str) {
    let mut inflight = handles.waits.inflight.lock().await;
    inflight.remove(session_id);
}

/// The completion notice (ADR-0043 amendment 2026-09-21): the streaming card is
/// patched in place, which pushes no notification and does not bump the
/// conversation — so reply to the requester's message to notify them. Groups
/// notify on every turn (`[bridge] group_completion_notice`); p2p only for a
/// long task (`[bridge] long_task_notice`, past the injected threshold), where
/// "long" is the one event worth surfacing even though the user was presumably
/// around.
///
/// A free function because every end of a turn calls it: `finish` for a turn
/// that ended normally, the out-of-turn drain follow (#284) when the turn it
/// inherited actually ends, and Session Sync when a yielded card's quiet true
/// end settles in place (ADR-0060). A drain hand-off keeps the ORIGINAL turn's
/// start, so the long-task threshold measures the whole run; a retry re-attach
/// (#393) arms the follow with "now" instead, because the original turn's
/// start is no longer known there; the quiet true end reads the start the card
/// recorded at turn start. The copy follows the ending's [`Disposition`]
/// (#394): ✅ for the true end, ⏹ for a deliberate `/stop`, ❌ for a failure —
/// and the disposition's classification is the refusal, so a Waiting yield
/// (whose true end is not reached, ADR-0059) and an Unreceived ending
/// (ADR-0062) can never be announced even by a caller that forgot to guard.
pub(crate) async fn send_completion_notice(
    cards: &CardsHandle,
    platform: &Arc<dyn crate::feishu::Platform>,
    rules: &NoticeRules,
    session_id: &str,
    started_at: std::time::Instant,
    disposition: &Disposition,
) {
    // Not an ending the notice may announce: stay silent before any lookup.
    let Some(text) = disposition.notice_copy() else {
        return;
    };
    if !(rules.group_completion_notice || rules.long_task_notice) {
        return;
    }
    let notice = {
        let cards = cards.cards.lock().await;
        cards.get(session_id).map(|c| &c.acc).and_then(|a| {
            let requester = a.requester_open_id.clone()?;
            let reply_to = a.reply_to_message_id.clone()?;
            let long_task =
                started_at.elapsed() >= std::time::Duration::from_millis(rules.long_task_notice_ms());
            if !(a.is_group && rules.group_completion_notice
                || !a.is_group && rules.long_task_notice && long_task)
            {
                return None;
            }
            Some((reply_to, requester, a.is_group))
        })
    };
    if let Some((reply_to, requester, is_group)) = notice {
        // Best-effort @-mention: the display name needs the contact API
        // (permission granted). On any lookup failure cola falls back to a
        // plain reply, which still notifies the message author. p2p needs no
        // @ — the reply itself is the notification.
        let name = if is_group {
            platform.user_name(&requester).await.unwrap_or(None)
        } else {
            None
        };
        if let Err(e) = platform
            .reply_completion_notice(&reply_to, &requester, name.as_deref(), text)
            .await
        {
            tracing::warn!("completion notice: {}", e);
        }
    }
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
            .and_then(|card| card.acc.staged_cursors.last().map(|staged| staged.cursor.clone()))
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
            card.acc.card_state = state;
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
            card.acc
                .staged_cursors
                .last()
                .map(|staged| state::StagedCursorId {
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
            card.acc.title = title.to_string();
        }
    }

    /// Set the card's reply target.
    pub(crate) async fn set_reply_target(cards: &CardsHandle, session_id: &str, reply_to: &str) {
        if let Some(card) = cards.cards.lock().await.get_mut(session_id) {
            card.acc.reply_to_message_id = Some(reply_to.to_string());
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
            card.acc.requester_open_id = Some(requester_open_id.to_string());
            card.acc.is_group = is_group;
            card.acc.turn_generation = Some(generation);
        }
    }

    /// Record the answering model and its provider.
    pub(crate) async fn set_model(cards: &CardsHandle, session_id: &str, provider: &str, model: &str) {
        if let Some(card) = cards.cards.lock().await.get_mut(session_id) {
            card.acc.provider_id = Some(provider.to_string());
            card.acc.model_id = Some(model.to_string());
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
            .map(|c| c.acc.card_state.clone())
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
            fresh.acc.cursor,
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

        send_completion_notice(
            &app.cards_handle(),
            &app.feishu,
            &app.turn_config().notice_rules(),
            "ses_test",
            std::time::Instant::now(),
            &Disposition::Waiting,
        )
        .await;
        assert!(
            !platform.calls.lock().await.iter().any(|call| matches!(
                call,
                crate::bridge::test_support::PlatformCall::CompletionNotice { .. }
            )),
            "a waiting disposition must never be announced"
        );

        send_completion_notice(
            &app.cards_handle(),
            &app.feishu,
            &app.turn_config().notice_rules(),
            "ses_test",
            std::time::Instant::now(),
            &Disposition::Done,
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
        let footer_variant = cards.get("ses_a").and_then(|c| c.acc.variant.clone());
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
            acc.acc.variant.as_deref(),
            Some("high"),
            "the footer reads the transcript's variant"
        );
        assert_eq!(acc.acc.model_id.as_deref(), Some("deepseek-v4-flash"));
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
            cards.get("ses_a").and_then(|c| c.acc.variant.clone()).is_none(),
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
