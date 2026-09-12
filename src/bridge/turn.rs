use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::bridge::App;
use crate::bridge::handler::image_inputs;
use crate::bridge::render::{flush_card, render_new_turn_parts, render_parts, render_poll_loop};
use crate::bridge::streaming::StreamAccumulator;
use crate::config::ThreadKey;
use crate::feishu::client::ImageAttachment;
use crate::opencode;

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
/// - `finish` — reconcile the final parts, Turn Footer, cover sync, group
///   notice, and release the guard.
pub(crate) struct Turn {
    session_id: String,
    thread_key: ThreadKey,
    text: String,
    cola_message_id: String,
    images: Vec<ImageAttachment>,
    epoch_ms: i64,
    /// The variant the last attempt actually sent, captured at send time —
    /// what the Turn Footer shows.
    turn_variant: Option<String>,
}

impl Turn {
    /// Run one prompt end-to-end: `start` → `attempt` (→ `recreate` + `attempt`
    /// on a stale mapping) → `finish`. The only public entry; the phases are
    /// internal seams.
    pub(crate) async fn run(app: &Arc<App>, ctx: PromptContext) -> crate::error::Result<()> {
        let Some(mut turn) = Self::start(app, ctx).await? else {
            return Ok(()); // busy: start already answered
        };
        let mut prompt_resp = turn.attempt(app).await;
        // The mapped session may not exist on the current server — e.g. it was
        // created in an old, now-abandoned store. Clear the mapping, create a
        // fresh session and retry once.
        if prompt_resp.as_ref().is_err_and(|e| e.is_session_not_found()) {
            tracing::warn!("session {} not found on the server; recreating", turn.session_id);
            turn.recreate(app).await?;
            prompt_resp = turn.attempt(app).await;
        }
        turn.finish(app, &prompt_resp).await;
        Ok(())
    }

    /// The busy guard, the Loading card (fresh reply or a reset of the retry's
    /// existing card) and the fresh accumulator this turn streams into.
    /// `Ok(None)` means another prompt holds the session — already answered.
    async fn start(app: &Arc<App>, ctx: PromptContext) -> crate::error::Result<Option<Turn>> {
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
        let cola_message_id = cola_message_id.unwrap_or_else(crate::opencode::client::cola_message_id);
        // Serialize prompts per session: if one is already in flight, don't let
        // a second message overwrite its accumulator (the two would race on the
        // same card). Reply with a notice only when we own a fresh message.
        {
            let mut inflight = app.inflight.lock().await;
            if inflight.contains(&session_id) {
                drop(inflight);
                if existing_card_id.is_none() {
                    let _ = app
                        .feishu
                        .reply_text(&message_id, "⏳ 上一条消息还在处理中，请稍等它完成后重发。")
                        .await;
                }
                return Ok(None);
            }
            inflight.insert(session_id.clone());
        }

        let loading = crate::feishu::card::CardBuilder::new()
            .with_state(crate::feishu::card::CardState::Loading)
            .with_subtitle(&subtitle)
            .build();
        let new_card_id = match existing_card_id {
            Some(cid) => {
                // Retry: reset the SAME card to Loading instead of replying a new one.
                if let Err(e) = app.feishu.update_message(&cid, &loading).await {
                    tracing::warn!("retry: reset card failed: {}", e);
                }
                Some(cid)
            }
            None => Some(app.feishu.reply_card(&message_id, &loading).await?),
        };
        let epoch_ms = chrono::Utc::now().timestamp_millis();
        {
            // Fresh accumulator per prompt: reuse leaks stale text/tools from the
            // previous turn into the next card. The card's IDENTITY (the
            // message id) carries over — only the content is reset.
            let session_dir = {
                let store = app.sessions.lock().await;
                store
                    .entry_for_session(&session_id)
                    .map(|e| e.directory.clone())
                    .unwrap_or_default()
            };
            let mut acc = StreamAccumulator::new(&subtitle);
            acc.reply_to_message_id = Some(message_id.clone());
            acc.session_id = Some(session_id.clone());
            acc.submit_epoch_ms = Some(epoch_ms);
            // The id this turn's user message carries, so a later retry reuses
            // it (ADR-0026) — the server deduplicates by id.
            acc.cola_message_id = Some(cola_message_id.clone());
            // Full original prompt, so the error-card "retry" can re-submit it.
            acc.prompt = Some(text.clone());
            acc.requester_open_id = requester_open_id.clone();
            acc.is_group = is_group;
            acc.attach_work_context(&session_dir).await;
            let mut cards = app.cards.lock().await;
            cards.insert(
                session_id.clone(),
                crate::bridge::streaming::CardSession::new(acc, new_card_id),
            );
        }

        Ok(Some(Turn {
            session_id,
            thread_key,
            text,
            cola_message_id,
            images,
            epoch_ms,
            turn_variant: None,
        }))
    }

    /// Send one attempt with the incremental renderer attached. The overrides
    /// are captured at send time (ADR-0019), and the renderer always stops
    /// before the response is returned so a retry starts its own cleanly.
    async fn attempt(&mut self, app: &Arc<App>) -> crate::error::Result<opencode::client::PromptResponse> {
        let render = RenderPoll::spawn(app, &self.session_id, self.epoch_ms);
        // Capture the variant actually sent this turn AT SEND TIME, not at
        // finalization: a `/think` issued mid-generation must not retro-tag the
        // card of a turn that was sent without it (same "capture at turn start"
        // rule as the work-context 📁 half, ADR-0019).
        self.turn_variant = app.session_variant_override(&self.session_id).await;
        let model = app.session_model_override(&self.session_id).await;
        let agent = app.session_agent_override(&self.session_id).await;
        let prompt_resp = app
            .opencode
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
        prompt_resp
    }

    /// Replace the dead mapping with a fresh session on the current server —
    /// never fall through to another stale mapping for the thread. Reuses the
    /// dead session's directory so the user keeps working in the same project,
    /// keeps its topic creation messages (ADR-0023) so the injection guard
    /// survives, and carries the turn's live state (card accumulator, inflight
    /// guard, cover title) across to the fresh session.
    async fn recreate(&mut self, app: &Arc<App>) -> crate::error::Result<()> {
        let old_entry = app.remove_session(&self.session_id).await?;
        let directory = old_entry
            .as_ref()
            .and_then(|e| (!e.directory.is_empty()).then_some(e.directory.clone()))
            .unwrap_or_else(|| app.default_session_directory());
        let fresh_id = app
            .create_fresh_session(
                &self.thread_key,
                &self.text,
                directory,
                old_entry.as_ref().and_then(|e| e.topic_anchor.clone()),
                old_entry.as_ref().and_then(|e| e.topic_root.clone()),
            )
            .await?;
        // Re-key the session's live card (accumulator + card identity in one
        // CardSession) from the dead session to the new one — a single remap
        // instead of three maps kept in lockstep.
        {
            let mut cards = app.cards.lock().await;
            if let Some(card) = cards.remove(&self.session_id) {
                cards.insert(fresh_id.clone(), card);
            }
        }
        {
            let mut inflight = app.inflight.lock().await;
            inflight.remove(&self.session_id);
            inflight.insert(fresh_id.clone());
        }
        // The cover card belongs to the topic, not the dead session — move its
        // recorded title to the fresh session so the title-sync hook keeps
        // patching the card after the recreate (ADR-0023).
        {
            let mut covers = app.cover_titles.lock().await;
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
    async fn finish(
        &mut self,
        app: &Arc<App>,
        prompt_resp: &crate::error::Result<opencode::client::PromptResponse>,
    ) {
        let prompt_err = match prompt_resp {
            Ok(r) => r.error.clone(),
            Err(e) => Some(e.to_string()),
        };

        // Reconcile: render any parts the incremental poll missed, then mark the
        // card Done (or Error). Fall back to the response parts if the fetch fails.
        let final_msgs = app.opencode.messages(&self.session_id).await.ok();
        {
            let mut cards = app.cards.lock().await;
            if let Some(acc) = cards.get_mut(&self.session_id).map(|c| &mut c.acc) {
                if let Ok(resp) = prompt_resp {
                    let mut rendered = false;
                    if let Some(msgs) = &final_msgs {
                        rendered = render_new_turn_parts(acc, msgs, self.epoch_ms);
                    }
                    if !rendered {
                        render_parts(acc, &resp.parts);
                    }
                }
                // Capture the answering model + token usage from the LATEST
                // assistant message unconditionally — the render dedup may have
                // captured them before the message carried its final tokens.
                if let Some(msgs) = &final_msgs {
                    let latest_assistant = msgs.iter().rfind(|m| m.info.role.as_deref() == Some("assistant"));
                    if let Some(m) = latest_assistant {
                        if let Some(model_id) = &m.info.model_id {
                            acc.model_id = Some(model_id.clone());
                        }
                        if let Some(provider_id) = &m.info.provider_id {
                            acc.provider_id = Some(provider_id.clone());
                        }
                        if let Some(tokens) = &m.info.tokens {
                            acc.context_tokens = tokens.context_used();
                        }
                    }
                }
                // The footer model line shows `provider/model@variant`: the
                // server reports the model but not the variant, so the variant
                // comes from what cola actually sent this turn.
                acc.variant = self.turn_variant.clone();
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
        crate::bridge::streaming::refresh_work_context(&app.core, &self.session_id).await;
        // Compute the context-usage ratio for the card footer (input tokens ÷
        // the model's context window), then flush so the footer is on the card.
        if prompt_err.is_none() {
            let info = {
                let cards = app.cards.lock().await;
                cards.get(&self.session_id).map(|c| &c.acc).and_then(|a| {
                    match (&a.provider_id, &a.model_id, a.context_tokens) {
                        (Some(p), Some(m), input) if input > 0 => Some((p.clone(), m.clone(), input)),
                        _ => None,
                    }
                })
            };
            if let Some((provider, model, input)) = info
                && let Ok(Some(window)) = app.opencode.model_context_window(&provider, &model).await
                && window > 0
            {
                let ratio = (input as f64 / window as f64).clamp(0.0, 1.0);
                let mut cards = app.cards.lock().await;
                if let Some(acc) = cards.get_mut(&self.session_id).map(|c| &mut c.acc) {
                    acc.context_ratio = Some(ratio);
                }
            }
        }
        flush_card(&app.core, &self.session_id).await;

        // Topic cover card (ADR-0023): once the server holds a real title for
        // the session — auto-generated after the first exchange, or set by
        // `/name` — patch the cover card (the thread root) in place, so the
        // chat-list topic entry stays current. Best effort; failures only log.
        if prompt_err.is_none() {
            let settled = crate::bridge::command::sync_topic_cover_title(&app.core, &self.session_id).await;
            // The auto-title can still be in flight when a short turn ends
            // (the title agent races the turn); only then retry with backoff,
            // so the cover follows even if the user stops here (ADR-0023).
            if !settled {
                crate::bridge::command::spawn_cover_title_retry(&app.core, &self.session_id);
            }
        }

        // No external-sync baseline is recorded here (ADR-0026): authorship is
        // carried by this turn's `msg_cola_` message id, and the external-message
        // poller alone owns the Sync Watermark, so a failed/errored turn can no
        // longer leave a stale watermark that makes cola's own message look
        // external after a server heal.

        // Group completion notice: the streaming card is patched in place, which
        // pushes no new notification — so reply to the requester's message so
        // Feishu notifies them. p2p chats don't need it (the reply lands in the
        // conversation directly).
        if app.group_completion_notice {
            let notice = {
                let cards = app.cards.lock().await;
                cards.get(&self.session_id).map(|c| &c.acc).and_then(|a| {
                    if !a.is_group {
                        return None;
                    }
                    let requester = a.requester_open_id.clone()?;
                    let reply_to = a.reply_to_message_id.clone()?;
                    Some((
                        reply_to,
                        requester,
                        a.card_state == crate::feishu::card::CardState::Error,
                    ))
                })
            };
            if let Some((reply_to, requester, is_error)) = notice {
                let text = if is_error {
                    "❌ 上一条请求处理出错了，可点击卡片上的「重试」。"
                } else {
                    "✅ 已完成。"
                };
                // Best-effort @-mention: the display name needs the contact API
                // (permission granted). On any lookup failure cola falls back to
                // a plain reply, which still notifies the message author.
                let name = app.feishu.user_name(&requester).await.unwrap_or(None);
                if let Err(e) = app
                    .feishu
                    .reply_completion_notice(&reply_to, &requester, name.as_deref(), text)
                    .await
                {
                    tracing::warn!("group completion notice: {}", e);
                }
            }
        }

        self.release(app).await;
        // Permissions are handled by the independent poller spawned in App::run,
        // so a prompt blocked on a permission still gets its card shown.
    }

    /// Release this turn's busy guard. Idempotent.
    async fn release(&self, app: &Arc<App>) {
        let mut inflight = app.inflight.lock().await;
        inflight.remove(&self.session_id);
    }
}

/// One attempt's incremental renderer: the poll loop plus its stop flag. Owns
/// the spawn/stop pairing so an attempt cannot leak a running poll loop.
struct RenderPoll {
    done: Arc<AtomicBool>,
    handle: tokio::task::JoinHandle<()>,
}

impl RenderPoll {
    fn spawn(app: &Arc<App>, session_id: &str, epoch_ms: i64) -> Self {
        let done = Arc::new(AtomicBool::new(false));
        let core = Arc::clone(&app.core);
        let sid = session_id.to_string();
        let flag = Arc::clone(&done);
        let handle = tokio::spawn(async move {
            render_poll_loop(&core, sid, epoch_ms, flag).await;
        });
        Self { done, handle }
    }

    async fn stop(self) {
        self.done.store(true, Ordering::SeqCst);
        let _ = self.handle.await;
    }
}
