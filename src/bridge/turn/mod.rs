mod flush;
mod follow;
mod render;
mod state;

/// The one card-session type the coordinator's map holds. Its accumulator and
/// identity chain stay private to the Turn module: every read/write goes
/// through the interface above (spec #298, A3).
pub(crate) use state::CardSession;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tracing::Instrument;

use crate::bridge::handler::image_inputs;
use crate::bridge::handles::{CardsHandle, SessionsHandle, TurnHandles};
use crate::bridge::span;
use crate::bridge::turn::state::StreamAccumulator;
use crate::config::ThreadKey;
use crate::feishu::client::ImageAttachment;
use crate::opencode;
use crate::opencode::types::{SessionMessage, SessionStatus};

/// How long one Backend read in the post-prompt drain may take before it is
/// abandoned. The drain's own bound caps this further per call: a hung
/// Backend must not hold the card (and the inflight guard) past the drain
/// deadline, and `/stop` must be observable within one bounded request.
const DRAIN_REQUEST_TIMEOUT_MS: u64 = 30_000;

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
/// fresh message and by the error-card "retry" action (which reuses the
/// existing card id + stored prompt).
pub(crate) struct PromptContext {
    pub(crate) session_id: String,
    pub(crate) thread_key: ThreadKey,
    pub(crate) text: String,
    pub(crate) message_id: String,
    pub(crate) subtitle: String,
    pub(crate) existing_card_id: Option<String>,
    pub(crate) requester_open_id: Option<String>,
    pub(crate) is_group: bool,
    /// The id cola assigned to this turn's user message (`msg_cola_…`,
    /// ADR-0026). Set when this run re-submits a previous attempt (error-card
    /// retry) so the server deduplicates by id; None means a fresh message,
    /// and the turn generates a new id.
    pub(crate) cola_message_id: Option<String>,
    /// Downloaded images attached to this turn (Image Attachments).
    pub(crate) images: Vec<ImageAttachment>,
}

/// One user→assistant exchange (CONTEXT.md: Turn). Owns the per-turn state and
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

    /// The busy guard, the Loading card (fresh reply or a reset of the retry's
    /// existing card) and the fresh accumulator this turn streams into.
    /// `Ok(None)` means another prompt holds the session — already answered.
    async fn start(handles: &TurnHandles, ctx: PromptContext) -> crate::error::Result<Option<Turn>> {
        let PromptContext {
            session_id,
            thread_key,
            text,
            message_id,
            subtitle,
            existing_card_id,
            requester_open_id,
            is_group,
            cola_message_id,
            images,
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
                if existing_card_id.is_none() {
                    let _ = handles
                        .platform
                        .reply_text(&message_id, "⏳ 上一条消息还在处理中，请稍等它完成后重发。")
                        .await;
                }
                return Ok(None);
            }
            inflight.insert(session_id.clone());
        }
        // A fresh Turn supersedes any `/stop` from an earlier one: the drain
        // marker (ADR-0043) is per-session and sticky until the next turn, so
        // clearing it here keeps a past stop from silencing this turn's drain.
        handles.waits.stopped_sessions.lock().await.remove(&session_id);

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
        let mut acc = StreamAccumulator::new(&subtitle);
        acc.reply_to_message_id = Some(message_id.clone());
        acc.session_id = Some(session_id.clone());
        // The id this turn's user message carries, so a later retry reuses
        // it (ADR-0026) — the server deduplicates by id.
        acc.cola_message_id = Some(cola_message_id.clone());
        // Full original prompt, so the error-card "retry" can re-submit it.
        acc.prompt = Some(text.clone());
        acc.requester_open_id = requester_open_id.clone();
        acc.is_group = is_group;
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

        let loading = crate::feishu::card::shell::CardBuilder::new()
            .with_state(crate::feishu::card::CardState::Loading)
            .with_subtitle(&subtitle)
            .build();
        let new_card_id = match existing_card_id {
            Some(cid) => {
                // Retry: reset the SAME card to Loading instead of replying a new one.
                if let Err(e) = handles.cards.feishu.update_message(&cid, &loading).await {
                    tracing::warn!("retry: reset card failed: {}", e);
                }
                Some(cid)
            }
            None => match handles.cards.feishu.reply_card(&message_id, &loading).await {
                Ok(id) => Some(id),
                Err(e) => {
                    // The turn never started: drop the just-inserted card
                    // session and release the guard, so nothing leaks and the
                    // session does not look busy until a restart.
                    handles.cards.cards.lock().await.remove(&session_id);
                    release_inflight(handles, &session_id).await;
                    return Err(e);
                }
            },
        };

        if let Some(cid) = new_card_id {
            let mut cards = handles.cards.cards.lock().await;
            if let Some(card) = cards.get_mut(&session_id) {
                card.card_message_id = Some(cid);
            }
        }

        // The work context (ADR-0019) is captured before the prompt runs but
        // AFTER the card is live and its id known: the git read neither delays
        // the loading card nor keeps the session card-less, and it runs outside
        // the cards lock.
        let session_dir = handles
            .sessions
            .entry_for_session(&session_id)
            .await
            .map(|e| e.directory.clone())
            .unwrap_or_default();
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
            started_at: std::time::Instant::now(),
            stop_finalization_logged: false,
        }))
    }

    /// Send one attempt with the incremental renderer attached. The overrides
    /// are captured at send time (ADR-0019), and the renderer always stops
    /// before the response is returned so a retry starts its own cleanly.
    async fn attempt(
        &mut self,
        handles: &TurnHandles,
    ) -> crate::error::Result<opencode::types::PromptResponse> {
        let render = render::RenderPoll::spawn(handles, &self.session_id, &self.thread_key);
        // Capture the variant actually sent this turn AT SEND TIME, not at
        // finalization: a `/think` issued mid-generation must not retro-tag the
        // card of a turn that was sent without it (same "capture at turn start"
        // rule as the work-context 📁 half, ADR-0019).
        self.turn_variant = handles.sessions.variant_override(&self.session_id).await;
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
        // The poll runs only while the prompt call is in flight. Should the
        // run have ended with a Supplement already queued — or a new Turn
        // started by one — the post-prompt drain takes the polling over on the
        // same injected cadence (ADR-0043).
        prompt_resp
    }

    /// Replace the dead mapping with a fresh session on the current server —
    /// never fall through to another stale mapping for the thread. Reuses the
    /// dead session's directory so the user keeps working in the same project,
    /// keeps its topic creation messages (ADR-0023) so the injection guard
    /// survives, and carries the turn's live state (card accumulator, inflight
    /// guard, cover title) across to the fresh session.
    async fn recreate(&mut self, handles: &TurnHandles) -> crate::error::Result<()> {
        let old_entry = match handles.sessions.remove_session(&self.session_id).await {
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
        let fresh_id = match handles
            .sessions
            .create_fresh_session(
                &handles.backend,
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
    async fn finish(
        &mut self,
        handles: &TurnHandles,
        prompt_resp: &crate::error::Result<opencode::types::PromptResponse>,
    ) {
        // Post-prompt drain + the pre-finalization re-check: a Supplement
        // racing the drain's exit is drained here rather than dropped, and one
        // that lands after the release becomes a normal new Turn on the
        // handler's not-busy path.
        let drain_outcome = self.drain_after_prompt(handles).await;

        let prompt_err = match prompt_resp {
            Ok(r) => r.error.clone(),
            Err(e) => Some(e.to_string()),
        };

        // #284: a drain bound reached while the session is still running is NOT
        // completion. The turn ends as usual (the guard is released below, so
        // the next message is a normal new Turn), but its card must not be
        // marked Done from a snapshot that still shows running tools: it is
        // handed to the out-of-turn follow, which keeps rendering the SAME
        // accumulator and card chain until the session reports non-busy (or the
        // follow's own ceiling ends it in Error). The anchor is the follow's
        // identity: without one there is no card content to follow, so the
        // turn ends the normal way.
        let follow_anchor = if prompt_err.is_none() && drain_outcome == Some(DrainState::Running) {
            Turn::armed_turn_anchor(&handles.cards, &self.session_id).await
        } else {
            None
        };
        let follow = follow_anchor.is_some();

        // #187: a turn that ended without completing (abort, interrupt, prompt
        // error) leaves every still-pending Permission/Question behind with a
        // dead tool fiber. Reject them here, so their blocks become `🚫 已拒绝`
        // receipts on the card instead of ghosting onto the next turn (#177).
        // Only an unfinished turn does this: a completed one has nothing
        // pending, and a request that outlived a healthy turn (external or
        // concurrent work) is not this turn's to deny.
        if prompt_err.is_some() {
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
        // the messages themselves, so the follow's renders keep them current.
        if let Some(card) = handles.cards.cards.lock().await.get_mut(&self.session_id) {
            card.acc.variant = self.turn_variant.clone();
        }

        // A followed card is NOT finalized here: the follow owns it now. Its
        // render ticks keep the footer's context window current, and its own
        // finalization refreshes the work context and flushes. Everything
        // below the guard is the normal end of a turn.
        if !follow {
            // Reconcile: render any parts the incremental poll missed, then mark
            // the card Done (or Error). Fall back to the response parts if the
            // fetch fails.
            let final_msgs = handles.backend.messages(&self.session_id).await.ok();
            {
                let mut cards = handles.cards.cards.lock().await;
                if let Some(acc) = cards.get_mut(&self.session_id).map(|c| &mut c.acc) {
                    if let Ok(resp) = prompt_resp {
                        let mut rendered = false;
                        if let Some(msgs) = &final_msgs {
                            rendered = render::render_new_turn_parts(acc, msgs);
                        }
                        if !rendered {
                            render::render_parts(acc, &resp.parts);
                        }
                    }
                    // Capture the answering model + token usage from the LATEST
                    // assistant message unconditionally — the render dedup may have
                    // captured them before the message carried its final tokens.
                    // Usage only when nonzero: an in-flight step's message carries
                    // zeros and must not wipe the last completed step's figure.
                    if let Some(msgs) = &final_msgs {
                        let latest_assistant =
                            msgs.iter().rfind(|m| m.info.role.as_deref() == Some("assistant"));
                        if let Some(m) = latest_assistant {
                            if let Some(model_id) = &m.info.model_id {
                                acc.model_id = Some(model_id.clone());
                            }
                            if let Some(provider_id) = &m.info.provider_id {
                                acc.provider_id = Some(provider_id.clone());
                            }
                            if let Some(tokens) = &m.info.tokens {
                                let used = tokens.context_used();
                                if used > 0 {
                                    acc.context_tokens = used;
                                }
                            }
                        }
                    }
                    if let Some(err) = &prompt_err {
                        acc.error = Some(err.clone());
                        acc.card_state = crate::feishu::card::CardState::Error;
                    } else {
                        acc.card_state = crate::feishu::card::CardState::Done;
                    }
                    tracing::info!(
                        "final render: fetched_msgs={} text={} reasoning={} tools={} rendered_parts={} error={}",
                        final_msgs.as_ref().map(|m| m.len()).unwrap_or(0),
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
        // real end), not at the drain bound.
        if !follow {
            send_completion_notice(handles, &self.session_id, self.started_at).await;
        }

        self.release(handles).await;
        // The guard is released before the follow arms, exactly as the old
        // finalization released it: a message arriving now is a normal new Turn
        // (which replaces the accumulator and ends the follow on its next tick).
        if let Some(turn_anchor_ms) = follow_anchor {
            tracing::info!(
                "turn drain: bound with session {} running; handing off to the follow",
                self.session_id
            );
            follow::spawn(
                handles,
                self.session_id.clone(),
                self.thread_key.clone(),
                self.directory.clone(),
                self.started_at,
                turn_anchor_ms,
            );
        }
        // Permissions are handled by the independent poller spawned in App::run,
        // so a prompt blocked on a permission still gets its card shown.
    }

    /// Release this turn's busy guard. Idempotent.
    async fn release(&self, handles: &TurnHandles) {
        release_inflight(handles, &self.session_id).await;
    }

    /// The post-prompt phase (ADR-0043): keep the renderer polling while the
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
    /// still pending (`Some(Running)` / `Some(Supplement)`), `None` when the
    /// drain settled or never saw anything pending. `finish` turns
    /// bound-with-`Running` into the out-of-turn follow (#284) instead of
    /// finalizing a card under a live session.
    async fn drain_after_prompt(&mut self, handles: &TurnHandles) -> Option<DrainState> {
        let last = self.drain(handles).await;
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
    /// observed — the caller must not read it as completion (#284); `None`
    /// means the drain settled, stopped, or never observed anything pending.
    async fn drain(&mut self, handles: &TurnHandles) -> Option<DrainState> {
        let poll_ms = handles.config.render_poll_ms();
        let deadline = drain_deadline(handles);
        let mut last: Option<DrainState> = None;
        loop {
            // The first check runs before any sleep: a Supplement that missed
            // the run is already on the Backend when the prompt returns.
            match self.drain_tick(handles, drain_request_timeout(deadline)).await {
                Some(DrainState::Settled) => return None,
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
        let msgs = self.drain_messages(handles, timeout_ms).await?;
        match self.drain_state(handles, &msgs, timeout_ms).await {
            DrainState::Settled => Some(DrainState::Settled),
            pending => {
                render::render_and_flush(
                    &handles.cards,
                    &handles.sessions,
                    &handles.backend,
                    &self.session_id,
                    &msgs,
                )
                .await;
                Some(pending)
            }
        }
    }

    /// What the drain must do next (ADR-0043): keep going while the session's
    /// run is still alive, or while the Backend's newest user message is a
    /// cola-authored Supplement newer than this turn's anchor with no
    /// assistant reply after it. `msgs` is the snapshot the caller just read.
    /// The Supplement is classified before the run state so the re-check can
    /// tell the racing Supplement apart from a session that is merely busy.
    async fn drain_state(
        &mut self,
        handles: &TurnHandles,
        msgs: &[SessionMessage],
        timeout_ms: u64,
    ) -> DrainState {
        // `/stop` interrupted this session's run: no answer is coming, so the
        // drain must end promptly instead of waiting out its bound on a
        // Supplement the abort left unanswered (no rendering may continue once
        // the session is stopped). The marker is cleared by the next Turn's
        // `start`. The finalization is logged once per Turn: the drain and the
        // re-check that follows it both read the same sticky marker.
        if handles
            .waits
            .stopped_sessions
            .lock()
            .await
            .contains(&self.session_id)
        {
            if !self.stop_finalization_logged {
                self.stop_finalization_logged = true;
                tracing::info!("turn drain: session {} was stopped; finalizing", self.session_id);
            }
            return DrainState::Settled;
        }
        // Capture the turn's server-time anchor from this snapshot if the
        // incremental poll never saw it — the supplement comparison is
        // meaningless without it, and a short turn's first poll only ever
        // runs here.
        let anchor_ms = {
            let mut cards = handles.cards.cards.lock().await;
            match cards.get_mut(&self.session_id) {
                Some(card) => {
                    render::capture_turn_anchor(&mut card.acc, msgs);
                    card.acc.turn_started_ms
                }
                None => None,
            }
        };
        // A Supplement the Backend has not answered yet. Its `msg_cola_` id is
        // authoritative (ADR-0026); the anchor is the server's own time for
        // this turn's user message (#190). Cola's clock never enters here.
        if let Some(anchor_ms) = anchor_ms {
            let newest_user = msgs
                .iter()
                .filter(|m| m.info.role.as_deref() == Some("user"))
                .filter_map(|m| m.info.time.as_ref().map(|t| (t.created, m.info.id.as_str())))
                .max_by_key(|(created, _)| *created);
            if let Some((created, id)) = newest_user
                && crate::opencode::parsing::is_cola_message_id(id)
                && created > anchor_ms
            {
                // No assistant message after it: either its new Turn has not
                // started yet or it is being answered — keep rendering either
                // way. (`/stop` was handled above; an aborted run leaves
                // nothing for this check to wait for.)
                let answered = msgs.iter().any(|m| {
                    m.info.role.as_deref() == Some("assistant")
                        && m.info
                            .time
                            .as_ref()
                            .map(|t| t.created >= created)
                            .unwrap_or(false)
                });
                if !answered {
                    return DrainState::Supplement;
                }
            }
        }
        // The run is still alive: parts keep coming.
        match crate::bridge::bounded_call(
            "turn drain session status",
            timeout_ms,
            handles
                .backend
                .session_status(&self.session_id, Some(&self.directory)),
        )
        .await
        {
            Some(Ok(Some(SessionStatus::Busy | SessionStatus::Retry))) => DrainState::Running,
            Some(Ok(_)) | None => DrainState::Settled,
            Some(Err(e)) => {
                tracing::warn!("turn drain session status: {}", e);
                DrainState::Settled
            }
        }
    }

    /// The drain's Backend read, bounded by the caller's per-call timeout so a
    /// hung Backend cannot hold the card (and the inflight guard) past the
    /// drain bound.
    async fn drain_messages(&self, handles: &TurnHandles, timeout_ms: u64) -> Option<Vec<SessionMessage>> {
        match crate::bridge::bounded_call(
            "turn drain messages",
            timeout_ms,
            handles.backend.messages(&self.session_id),
        )
        .await
        {
            Some(Ok(msgs)) => Some(msgs),
            Some(Err(e)) => {
                tracing::warn!("turn drain messages: {}", e);
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

/// Why a Card Chain split was requested: each cause writes its own receipt
/// line on the continuation (ADR-0043).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SplitKind {
    /// A Supplement landed below the live card (ADR-0043).
    Supplement,
    /// The user explicitly pulled the live card down with `/card`
    /// (ADR-0043, 2026-09-22 amendment).
    Pull,
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
        flush::flush_card_locked(cards, session_id).await;
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
            });
        }
        flush::flush_card_locked(cards, session_id).await;
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
        backend: &Arc<dyn opencode::Backend>,
        thread_key: &ThreadKey,
        text: &str,
    ) -> String {
        render::session_subtitle(sessions, backend, thread_key, text).await
    }

    /// Render a polled message snapshot into `session_id`'s live card, flushing
    /// when the content, header or context footer changed. Returns `None` when
    /// the session's accumulator vanished (the caller should stop).
    pub(crate) async fn render_and_flush(
        cards: &CardsHandle,
        sessions: &SessionsHandle,
        backend: &Arc<dyn opencode::Backend>,
        session_id: &str,
        msgs: &[SessionMessage],
    ) -> Option<(usize, usize, usize)> {
        render::render_and_flush(cards, sessions, backend, session_id, msgs).await
    }
}

/// The Instant Reminder facts a cola Turn's live card carries (ADR-0043): the
/// turn generation, the requester, and whether the prompt came from a group.
pub(crate) struct TurnPinSource {
    pub(crate) generation: u64,
    pub(crate) requester_open_id: String,
    pub(crate) is_group: bool,
}

/// The error-card retry fixture a failed turn's card carries: the original
/// prompt, its reply target, and the identity/thread facts a retry reuses.
pub(crate) struct TurnRetry {
    pub(crate) prompt: String,
    pub(crate) reply_to: String,
    pub(crate) subtitle: String,
    pub(crate) requester_open_id: Option<String>,
    pub(crate) is_group: bool,
    pub(crate) cola_message_id: Option<String>,
    pub(crate) card_message_id: Option<String>,
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

    /// Whether the session's card is still running (not Done/Error). The map's
    /// key alone does not mean a live card — a completed turn stays until the
    /// next Turn replaces it.
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

    /// The turn anchor of the session's armed renderer, if one is armed: the
    /// renderer identity both external arming paths compare their own turn's
    /// server time against.
    pub(crate) async fn armed_turn_anchor(cards: &CardsHandle, session_id: &str) -> Option<i64> {
        cards
            .cards
            .lock()
            .await
            .get(session_id)
            .and_then(|c| c.acc.turn_started_ms)
    }

    /// Whether the session's card has rendered any part — the external
    /// renderer's "partial reply" probe before it finalizes on timeout.
    pub(crate) async fn has_rendered_content(cards: &CardsHandle, session_id: &str) -> bool {
        cards
            .cards
            .lock()
            .await
            .get(session_id)
            .is_some_and(|c| !c.acc.rendered_parts.is_empty() || !c.acc.rendered_tool_states.is_empty())
    }

    /// Whether the session's card still carries an unfinished Tool Panel — a
    /// call whose status is `running` or `pending` (both render `⏳`). The
    /// drain follow's Done decision waits for these to settle (#284): the card
    /// must never read `✅ 完成` over a `⏳` panel.
    pub(crate) async fn has_live_tools(cards: &CardsHandle, session_id: &str) -> bool {
        cards.cards.lock().await.get(session_id).is_some_and(|c| {
            c.acc
                .tools
                .values()
                .any(crate::feishu::card::tool_render::ToolPanel::is_live)
                || c.acc
                    .todo_panel
                    .as_ref()
                    .is_some_and(crate::feishu::card::tool_render::ToolPanel::is_live)
        })
    }

    /// Mark the session's card Done in place (no flush).
    pub(crate) async fn mark_done(cards: &CardsHandle, session_id: &str) {
        if let Some(card) = cards.cards.lock().await.get_mut(session_id) {
            card.acc.card_state = crate::feishu::card::CardState::Done;
        }
    }

    /// Re-point the live card identity at a new message (ADR-0028: a re-adopt
    /// mid-turn sends a fresh snapshot; the follow renderer keeps updating the
    /// new card instead of the old one). The content is untouched.
    pub(crate) async fn repoint_card(cards: &CardsHandle, session_id: &str, message_id: &str) {
        if let Some(card) = cards.cards.lock().await.get_mut(session_id) {
            card.repoint(message_id);
        }
    }

    /// Refresh the live card's work context at turn end (ADR-0019): re-read the
    /// session directory's git state so the final card shows where the turn
    /// landed.
    pub(crate) async fn refresh_work_context(cards: &CardsHandle, session_id: &str) {
        state::refresh_work_context(cards, session_id).await;
    }

    /// Finalize an externally-rendered card: refresh its work context, mark it
    /// Done and flush it.
    pub(crate) async fn finalize_done(cards: &CardsHandle, session_id: &str) {
        Self::refresh_work_context(cards, session_id).await;
        Self::mark_done(cards, session_id).await;
        Self::flush_card(cards, session_id).await;
    }

    /// Finalize a followed card as Error: record `error`, mark it Error and
    /// flush it. Used by the out-of-turn drain follow (#284) when its ceiling is
    /// reached with the session still running — the card must never read Done
    /// while a tool panel is still running.
    pub(crate) async fn finalize_error(cards: &CardsHandle, session_id: &str, error: &str) {
        if let Some(card) = cards.cards.lock().await.get_mut(session_id) {
            card.acc.error = Some(error.to_string());
            card.acc.card_state = crate::feishu::card::CardState::Error;
        }
        Self::refresh_work_context(cards, session_id).await;
        Self::flush_card(cards, session_id).await;
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

    /// The error-card retry fixture a session's card carries: the original
    /// prompt, its reply target, subtitle, requester/chat facts and the card id
    /// to reset. `None` when the session has no card.
    pub(crate) async fn retry_request(cards: &CardsHandle, session_id: &str) -> Option<TurnRetry> {
        let live = cards.cards.lock().await;
        let card = live.get(session_id)?;
        Some(TurnRetry {
            prompt: card.acc.prompt.clone().unwrap_or_default(),
            reply_to: card.acc.reply_to_message_id.clone().unwrap_or_default(),
            subtitle: card.acc.title.clone(),
            requester_open_id: card.acc.requester_open_id.clone(),
            is_group: card.acc.is_group,
            cola_message_id: card.acc.cola_message_id.clone(),
            card_message_id: card.card_message_id.clone(),
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

    /// Resolve every live permission block whose request vanished (resolved by
    /// another client) into its Interaction Receipt; an item owned by a
    /// directory whose list failed, or that cola itself is answering, stays
    /// live (#130, #144). Returns the affected session ids — the sweep repaints
    /// each affected card so the receipt lands within one poll (ADR-0038).
    pub(crate) async fn resolve_vanished_permissions(
        cards: &CardsHandle,
        pending: &HashSet<String>,
        failed_dirs: &HashSet<String>,
        cola_claimed: &HashSet<String>,
    ) -> Vec<String> {
        Self::resolve_vanished(cards, pending, failed_dirs, cola_claimed, |block| {
            matches!(block, state::InteractionBlock::Permission(_))
        })
        .await
    }

    /// [`Self::resolve_vanished_permissions`] for the question kind.
    pub(crate) async fn resolve_vanished_questions(
        cards: &CardsHandle,
        pending: &HashSet<String>,
        failed_dirs: &HashSet<String>,
        cola_claimed: &HashSet<String>,
    ) -> Vec<String> {
        Self::resolve_vanished(cards, pending, failed_dirs, cola_claimed, |block| {
            matches!(block, state::InteractionBlock::Question(_))
        })
        .await
    }

    /// The shared sweep body: resolve every live block `own` accepts whose
    /// request vanished, over every card session. Returns the affected session
    /// ids.
    async fn resolve_vanished(
        cards: &CardsHandle,
        pending: &HashSet<String>,
        failed_dirs: &HashSet<String>,
        cola_claimed: &HashSet<String>,
        own: impl Fn(&state::InteractionBlock) -> bool,
    ) -> Vec<String> {
        let mut live = cards.cards.lock().await;
        let mut affected = Vec::new();
        for (session_id, card) in live.iter_mut() {
            if state::resolve_vanished_blocks(&mut card.acc, pending, failed_dirs, cola_claimed, &own) > 0 {
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

    /// Arm an external renderer's card: build the turn's accumulator, attach
    /// the work context, push the anchor text (the message preview / snapshot
    /// identity) just before the turn's server-time anchor so the reply's parts
    /// always insert below it, and insert the card session the render loop
    /// streams into. `variant` is the session's `/think` override captured at
    /// ARM time (ADR-0019).
    #[allow(clippy::too_many_arguments)] // the card's whole arming fixture
    pub(crate) async fn arm_external_render(
        cards: &CardsHandle,
        session_id: &str,
        card_id: &str,
        turn_anchor_ms: i64,
        subtitle: &str,
        session_dir: &str,
        variant: Option<String>,
        anchor_text: Option<&str>,
    ) {
        let mut acc = state::StreamAccumulator::new(subtitle);
        // The external message's server time is the turn's anchor: header date,
        // turn filter and renderer replacement guard all read it — one
        // server-clock value, and cola's clock is never part of the card
        // (#183, #190).
        acc.turn_started_ms = Some(turn_anchor_ms);
        acc.session_id = Some(session_id.to_string());
        acc.reply_to_message_id = Some(card_id.to_string());
        acc.attach_work_context(session_dir).await;
        acc.variant = variant;
        if let Some(text) = anchor_text.filter(|text| !text.is_empty()) {
            acc.push_text_at(Some(turn_anchor_ms - 1), text);
        }
        cards.cards.lock().await.insert(
            session_id.to_string(),
            state::CardSession::new(acc, Some(card_id.to_string())),
        );
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
/// A free function because both ends of a turn call it: `finish` for a turn
/// that ended normally, and the out-of-turn drain follow (#284) when the turn
/// it inherited actually ends — `started_at` stays the ORIGINAL turn's start,
/// so the long-task threshold measures the whole run.
async fn send_completion_notice(handles: &TurnHandles, session_id: &str, started_at: std::time::Instant) {
    if !(handles.config.group_completion_notice || handles.config.long_task_notice) {
        return;
    }
    let notice = {
        let cards = handles.cards.cards.lock().await;
        cards.get(session_id).map(|c| &c.acc).and_then(|a| {
            let requester = a.requester_open_id.clone()?;
            let reply_to = a.reply_to_message_id.clone()?;
            let long_task = started_at.elapsed()
                >= std::time::Duration::from_millis(handles.config.long_task_notice_ms());
            if !(a.is_group && handles.config.group_completion_notice
                || !a.is_group && handles.config.long_task_notice && long_task)
            {
                return None;
            }
            Some((
                reply_to,
                requester,
                a.is_group,
                a.card_state == crate::feishu::card::CardState::Error,
            ))
        })
    };
    if let Some((reply_to, requester, is_group, is_error)) = notice {
        let text = if is_error {
            "❌ 上一条请求处理出错了，可点击卡片上的「重试」。"
        } else {
            "✅ 已完成。"
        };
        // Best-effort @-mention: the display name needs the contact API
        // (permission granted). On any lookup failure cola falls back to a
        // plain reply, which still notifies the message author. p2p needs no
        // @ — the reply itself is the notification.
        let name = if is_group {
            handles.platform.user_name(&requester).await.unwrap_or(None)
        } else {
            None
        };
        if let Err(e) = handles
            .platform
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

    /// Clear the card's phase timer (a finalized-card fixture).
    pub(crate) async fn clear_phase(cards: &CardsHandle, session_id: &str) {
        if let Some(card) = cards.cards.lock().await.get_mut(session_id) {
            card.acc.current_phase = None;
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

    /// Set the turn's server-time anchor.
    pub(crate) async fn set_turn_anchor(cards: &CardsHandle, session_id: &str, anchor_ms: i64) {
        if let Some(card) = cards.cards.lock().await.get_mut(session_id) {
            card.acc.turn_started_ms = Some(anchor_ms);
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
            existing_card_id: None,
            requester_open_id: None,
            is_group: false,
            cola_message_id: None,
            images: Vec::new(),
        }
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
}
