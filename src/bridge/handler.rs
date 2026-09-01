use std::ops::Deref;
use std::sync::Arc;

use crate::bridge::command;
use crate::bridge::core::SharedCore;
use crate::bridge::render::{flush_card, render_new_turn_parts, render_parts, render_poll_loop};
use crate::bridge::streaming::StreamAccumulator;
use crate::config::{Config, ConversationKind, SessionEntry, ThreadKey};
use crate::feishu;
use crate::opencode;

/// Shown once when the first top-level message in a group auto-creates the
/// group's lobby session.
const GROUP_LOBBY_GUIDANCE: &str = "\
**已创建群会话** —— 这里就是本群的默认会话，可直接对话。
想为不同项目/任务分别开会话，请用：
- 在群里回复开启**话题**：每个话题是一个独立会话
- `/new [名字]` 开一个显式会话
- `/dir <路径>` 切换项目目录
- `/help` 查看全部命令";

// Re-exec cola itself with the ORIGINAL startup args, inheriting stdio so a
// shell log redirect (`cola ... > test.log 2>&1`) carries into the new process.
// The current process then calls `std::process::exit(0)` right after.

/// Convert downloaded platform images into OpenCode `ImageInput`s (data-URL
/// `file` parts). Empty for text-only turns; the retry path loses images (they
/// are not persisted on the accumulator) and sends text alone.
fn image_inputs(images: &[crate::feishu::client::ImageAttachment]) -> Vec<opencode::client::ImageInput> {
    use base64::Engine;
    images
        .iter()
        .map(|img| opencode::client::ImageInput {
            mime: img.mime.clone(),
            data_base64: base64::engine::general_purpose::STANDARD.encode(&img.data),
        })
        .collect()
}

/// Everything `run_prompt` needs for one turn. Built by `handle_prompt` for a
/// fresh message and by the error-card "retry" action (which reuses the
/// existing card id + stored prompt).
struct PromptContext {
    session_id: String,
    thread_key: ThreadKey,
    text: String,
    message_id: String,
    subtitle: String,
    existing_card_id: Option<String>,
    requester_open_id: Option<String>,
    is_group: bool,
    /// Downloaded images attached to this turn (Image Attachments).
    images: Vec<crate::feishu::client::ImageAttachment>,
}

/// The bridge coordinator. Owns the state shared by every flow ([`SharedCore`])
/// plus the per-flow modules that hold their own private state. `Deref`s to the
/// shared core so flows and callers can reach `app.sessions`, `app.opencode`,
/// etc. without threading a separate handle.
pub struct App {
    pub(crate) core: Arc<SharedCore>,
    /// Weak self-reference, set once inside `run` (which holds the Arc). Lets
    /// the `EventSink` trait impl (which only has `&self`) recover a
    /// `&Arc<App>` to hand to the inherent methods. `Weak` so it never keeps
    /// the app alive — no reference cycle; `OnceLock` because it is written
    /// exactly once, before any event can arrive.
    self_weak: std::sync::OnceLock<std::sync::Weak<App>>,
    /// Permission flow: owns `sent_cards`, polls pending requests, auto-accepts
    /// for `/autoaccept` sessions, and handles the "perm" card action.
    pub permission: super::request::RequestFlow,
    /// Question flow: owns `sent_cards` + the question kind's request/partial
    /// state, polls pending questions, and handles the "question" card action
    /// (answer / submit / reject).
    pub question: super::request::RequestFlow,
    /// External-message flow: owns `last_user_msg_epoch`, notifies Feishu when
    /// another shared-store client posts while cola is idle.
    pub external: super::external::ExternalFlow,
}

impl Deref for App {
    type Target = SharedCore;
    fn deref(&self) -> &Self::Target {
        &self.core
    }
}

#[async_trait::async_trait]
impl crate::bridge::EventSink for App {
    async fn handle_message(&self, msg: crate::bridge::IncomingMessage) {
        if let Some(app) = self.self_arc() {
            app.handle_message(msg).await;
        }
    }

    async fn handle_card_action(&self, value: serde_json::Value) -> Option<CardActionResult> {
        match self.self_arc() {
            Some(app) => app.handle_card_action(value).await,
            None => None,
        }
    }
}

/// What a card click produces: an optional replacement card (JSON 2.0, so it
/// stays update-compatible with the 2.0 interactive cards, see Feishu err
/// 200830) and an optional client Toast for instant feedback. `card: None` means
/// "keep the current card" — used when an interaction was answered inline inside
/// the streaming card, which re-renders itself on the next poll.
pub struct CardActionResult {
    pub card: Option<serde_json::Value>,
    pub toast: Option<String>,
}

/// The thread a card callback routes to. Every cola card button carries its
/// `chat_id` + `thread_id` in the value payload so the ack can route the choice
/// back to the right conversation.
fn thread_key_from_value(value: &serde_json::Value) -> crate::config::ThreadKey {
    crate::config::ThreadKey::new(
        value
            .get("chat_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        value
            .get("thread_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
    )
}

impl App {
    pub fn new(
        cfg: Config,
        opencode: Arc<dyn opencode::Backend>,
        feishu: Arc<dyn feishu::Platform>,
    ) -> anyhow::Result<Self> {
        let core = Arc::new(SharedCore::new(&cfg, opencode, feishu)?);
        Ok(Self {
            self_weak: std::sync::OnceLock::new(),
            permission: super::request::RequestFlow::new(Box::new(super::request::PermissionKind)),
            question: super::request::RequestFlow::new(Box::new(super::request::QuestionKind)),
            external: super::external::ExternalFlow::new(),
            core,
        })
    }

    /// Recover an `Arc<Self>` from the weak self-reference set by `run`. Used
    /// by the `EventSink` trait impl to hand a `&Arc<App>` to the inherent
    /// methods. None before `run` sets it (and after it returns).
    fn self_arc(&self) -> Option<Arc<Self>> {
        self.self_weak.get().and_then(|w| w.upgrade())
    }

    pub async fn run(self: Arc<Self>) -> anyhow::Result<()> {
        // Let the EventSink trait impl recover a &Arc<App> from &self.
        let _ = self.self_weak.set(Arc::downgrade(&self));
        // After a `/restart` or self-update, announce it in the chat that
        // requested it. `/update` writes kind="update" + the new version.
        let notify_path = command::restart_notify_path();
        if let Ok(raw) = std::fs::read_to_string(&notify_path)
            && let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw)
            && let Some(chat_id) = v.get("chat_id").and_then(|c| c.as_str())
        {
            let _ = std::fs::remove_file(&notify_path);
            let (title, body) = match v.get("kind").and_then(|k| k.as_str()) {
                Some("update") => {
                    let version = v.get("version").and_then(|s| s.as_str()).unwrap_or("");
                    (
                        format!("✅ 已更新到 {version}"),
                        format!("cola 已更新到 {version} 并重启完成。"),
                    )
                }
                _ => ("♻️ 已重启".to_string(), "cola 已重启完成。".to_string()),
            };
            let card = serde_json::json!({
                "schema": "2.0",
                "config": { "wide_screen_mode": true },
                "header": { "title": { "tag": "plain_text", "content": title }, "template": "green" },
                "body": { "elements": [ { "tag": "markdown", "content": body } ] }
            });
            match self.feishu.send_card("chat_id", chat_id, &card).await {
                Ok(_) => tracing::info!("announced restart in chat {}", chat_id),
                Err(e) => tracing::warn!("restart announce failed: {}", e),
            }
        }

        // Silent startup self-update check (ADR-0015): log when a new version
        // exists; never sends a card. Fire-and-forget — a dead network or a
        // rate limit only costs a debug log line.
        {
            tokio::spawn(async move {
                match crate::update::check().await {
                    Ok(crate::update::UpdateCheck::Available(info)) => {
                        tracing::info!(
                            "新版本 {} 可用（当前 {}）—— 发送 /update 更新。",
                            info.latest,
                            info.current
                        );
                    }
                    Ok(_) => {}
                    Err(e) => tracing::debug!("update check failed: {e}"),
                }
            });
        }

        // Discover cola's own open_id so @mentions of the bot can be recognised
        // and stripped from prompt text (Feishu delivers them as `@_user_N`).
        // Owned by the ws module now (see `feishu::ws`).

        let ws = Arc::clone(&self);
        let ws_feishu = Arc::clone(&self.feishu);
        let ws_state = Arc::new(feishu::ws::WsState::new());
        let perm_core = Arc::clone(&self.core);
        let question_core = Arc::clone(&self.core);
        let external_core = Arc::clone(&self.core);
        let reconnect_core = Arc::clone(&self.core);
        let perm_app = Arc::clone(&self);
        let question_app = Arc::clone(&self);
        let external_app = Arc::clone(&self);
        let ws_task = tokio::spawn(async move {
            let sink: Arc<dyn crate::bridge::EventSink> = ws;
            if let Err(e) = feishu::ws::event_loop(&sink, &ws_feishu, &ws_state).await {
                tracing::error!("WS: {}", e);
            }
        });
        // Permissions are not delivered on the global SSE (typed PubSub only),
        // and a prompt can be blocked on an unanswered permission forever, so the
        // poller must run independently of any single prompt lifecycle.
        let perm_task = tokio::spawn(async move {
            if let Err(e) = perm_app.permission.poll_loop(&perm_core).await {
                tracing::error!("Permission poller: {}", e);
            }
        });
        // Questions (the interactive `question` tool) work the same way: the AI
        // blocks until answered, the event never reaches the global SSE, so poll
        // and surface them as Feishu cards.
        let question_task = tokio::spawn(async move {
            if let Err(e) = question_app.question.poll_loop(&question_core).await {
                tracing::error!("Question poller: {}", e);
            }
        });
        // Notify Feishu when someone posts a message from another shared-store
        // client (e.g. OpenChamber) while cola is idle on that session.
        let external_task = tokio::spawn(async move {
            if let Err(e) = external_app.external.poll_loop(&external_core).await {
                tracing::error!("External message poller: {}", e);
            }
        });
        // The OpenCode server cola attaches to is managed by another tool that
        // can restart it (new pid/port/password). Re-detect a changed server so
        // cola reconnects instead of 502ing against the dead port forever.
        let reconnect_task = tokio::spawn(async move {
            if let Err(e) = crate::bridge::pollers::reconnect_poll_loop(&reconnect_core).await {
                tracing::error!("Reconnect poller: {}", e);
            }
        });
        tokio::try_join!(ws_task, perm_task, question_task, external_task, reconnect_task)?;
        Ok(())
    }

    pub async fn handle_message(self: &Arc<Self>, msg: crate::bridge::IncomingMessage) {
        let kind = ConversationKind::classify(&msg.chat_type, msg.thread_id.as_deref());
        let thread_key = kind.thread_key(&msg.chat_id, msg.thread_id.as_deref());
        if let Some(cmd) = command::parse_command(&msg.text) {
            // Unrecognized `/command`s are forwarded as prompts. Command
            // dispatch lives on the shared core, but the prompt pipeline is the
            // coordinator's — so Forward is routed here, before the command flow.
            if let command::Command::Forward(text) = cmd {
                let forward = crate::bridge::IncomingMessage {
                    message_id: msg.message_id,
                    chat_id: msg.chat_id,
                    chat_type: msg.chat_type,
                    thread_id: msg.thread_id,
                    parent_id: None,
                    text,
                    images: vec![],
                    requester_open_id: None,
                };
                if let Err(e) = self.handle_prompt(thread_key, forward, kind).await {
                    tracing::error!("Prompt: {}", e);
                }
                return;
            }
            if let Err(e) = command::handle_command(&self.core, cmd, thread_key, &msg.message_id, kind).await
            {
                tracing::error!("Cmd: {}", e);
            }
            return;
        }
        if let Err(e) = self.handle_prompt(thread_key, msg, kind).await {
            tracing::error!("Prompt: {}", e);
        }
    }

    pub(crate) async fn handle_prompt(
        self: &Arc<Self>,
        thread_key: ThreadKey,
        msg: crate::bridge::IncomingMessage,
        kind: ConversationKind,
    ) -> crate::error::Result<()> {
        let crate::bridge::IncomingMessage {
            message_id,
            chat_type,
            parent_id,
            text,
            images,
            requester_open_id,
            ..
        } = msg;
        // Lazy Start (ADR-0013): a prompt is the moment a server is needed.
        // Attach to an existing default-store server, or spawn an Owned Server
        // when none exists (unless `start_server = "never"`). Serverless means
        // the bot has no OpenCode to answer with — tell the user instead of
        // failing silently inside the prompt flow. Commands never trigger this
        // (so `/restart-opencode` still reports NoServer/NotOwned properly).
        match crate::bridge::pollers::ensure_server(&self.core).await {
            Ok(true) => {}
            Ok(false) => {
                let _ = self
                    .feishu
                    .reply_text(
                        &message_id,
                        "⚠️ 当前没有可用的 OpenCode server，且 `start_server = \"never\"`。\
                          \n请启动 OpenChamber（或手动 `opencode serve`），或把配置改为 `start_server = \"auto\"`。",
                    )
                    .await;
                return Ok(());
            }
            Err(e) => {
                tracing::warn!("ensure server failed: {}", e);
                let _ = self
                    .feishu
                    .reply_text(&message_id, &format!("⚠️ 启动 OpenCode server 失败：{e}"))
                    .await;
                return Ok(());
            }
        }
        let is_group = chat_type == "group";
        let mut text = text;
        let mut images = images;

        // Quoted Context: when this message replies to another, fetch the
        // parent and prepend it so the model sees what the reply answers —
        // including parents missing from session history (lobby-session switch,
        // compaction). Depth-1 with a short timeout; any failure degrades to
        // text-only (the pre-change behavior).
        if let Some(pid) = parent_id.as_deref() {
            let fetch = feishu::ws::quoted_context(&self.feishu, pid);
            let fetch = tokio::time::timeout(std::time::Duration::from_millis(1500), fetch);
            match fetch.await {
                Ok(Ok(ctx)) => {
                    if !ctx.text.is_empty() {
                        let capped: String = ctx.text.chars().take(2000).collect();
                        text = format!("[引用消息]:\n{}\n\n{}", capped, text);
                    }
                    // Quoted images come first, then the reply's own images.
                    images.splice(0..0, ctx.images);
                }
                Ok(Err(e)) => tracing::debug!("quoted context fetch failed for {}: {}", pid, e),
                Err(_) => tracing::debug!("quoted context fetch timed out for {}", pid),
            }
        }

        let (session_id, created) = self.get_or_create_session(&thread_key, &text).await?;

        // First message on a group's top level created a lobby session: reply
        // once with guidance so the user knows each topic isolates a session.
        if created && kind == ConversationKind::GroupLobby {
            self.feishu.reply_text(&message_id, GROUP_LOBBY_GUIDANCE).await?;
        }

        let subtitle = crate::bridge::render::session_subtitle(&self.core, &thread_key, &text).await;

        // Supplement path: if this session already has a turn in flight, don't
        // start a competing run_prompt (it would overwrite the running turn's
        // accumulator and race on the same card). Instead send the message
        // fire-and-forget via prompt_async — OpenCode persists it and the
        // running loop picks it up at the next tool boundary, merging it into
        // the current turn (original message preserved; model sees full
        // history). This is what lets the user append context mid-turn without
        // /stop and without interrupting a tool call.
        {
            let busy = self.inflight.lock().await.contains(&session_id);
            if busy {
                let image_inputs = image_inputs(&images);
                match self
                    .opencode
                    .prompt_async(
                        &session_id,
                        &text,
                        &image_inputs,
                        self.session_model_override(&session_id).await.as_ref(),
                        self.session_agent_override(&session_id).await.as_deref(),
                    )
                    .await
                {
                    Ok(()) => {
                        tracing::info!(
                            "supplement: session {} in-flight, message queued to merge into current turn",
                            session_id
                        );
                        if kind == ConversationKind::P2p || kind == ConversationKind::Topic {
                            let _ = self
                                .feishu
                                .reply_text(
                                    &message_id,
                                    "📨 已收到补充，将并入当前处理。若当前轮已结束，会作为下一条消息继续。",
                                )
                                .await;
                        }
                    }
                    Err(e) => {
                        tracing::warn!("supplement: prompt_async failed: {}", e);
                        let _ = self
                            .feishu
                            .reply_text(&message_id, "⚠️ 补充消息发送失败，请稍后重试。")
                            .await;
                    }
                }
                return Ok(());
            }
        }

        self.run_prompt(PromptContext {
            session_id,
            thread_key,
            text,
            message_id,
            subtitle,
            existing_card_id: None,
            requester_open_id,
            is_group,
            images,
        })
        .await
    }

    /// Run one prompt end-to-end: show a Loading card (either a fresh reply or a
    /// reset of an existing card when retrying), stream parts via the poll loop,
    /// then render the final Done/Error card. Shared by fresh messages and the
    /// error-card "retry" action.
    async fn run_prompt(self: &Arc<Self>, ctx: PromptContext) -> crate::error::Result<()> {
        let PromptContext {
            mut session_id,
            thread_key,
            text,
            message_id,
            subtitle,
            existing_card_id,
            requester_open_id,
            is_group,
            images,
        } = ctx;
        // Serialize prompts per session: if one is already in flight, don't let
        // a second message overwrite its accumulator (the two would race on the
        // same card). Reply with a notice only when we own a fresh message.
        {
            let mut inflight = self.inflight.lock().await;
            if inflight.contains(&session_id) {
                drop(inflight);
                if existing_card_id.is_none() {
                    let _ = self
                        .feishu
                        .reply_text(&message_id, "⏳ 上一条消息还在处理中，请稍等它完成后重发。")
                        .await;
                }
                return Ok(());
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
                if let Err(e) = self.feishu.update_message(&cid, &loading).await {
                    tracing::warn!("retry: reset card failed: {}", e);
                }
                Some(cid)
            }
            None => Some(self.feishu.reply_card(&message_id, &loading).await?),
        };
        let epoch_ms = chrono::Utc::now().timestamp_millis();
        {
            // Fresh accumulator per prompt: reuse leaks stale text/tools from the
            // previous turn into the next card. The card's IDENTITY (the
            // message id) carries over — only the content is reset.
            let session_dir = {
                let store = self.sessions.lock().await;
                store
                    .entry_for_session(&session_id)
                    .map(|e| e.directory.clone())
                    .unwrap_or_default()
            };
            let mut acc = StreamAccumulator::new(&subtitle);
            acc.reply_to_message_id = Some(message_id);
            acc.session_id = Some(session_id.clone());
            acc.submit_epoch_ms = Some(epoch_ms);
            // Full original prompt, so the error-card "retry" can re-submit it.
            acc.prompt = Some(text.clone());
            acc.requester_open_id = requester_open_id;
            acc.is_group = is_group;
            acc.directory = if session_dir.is_empty() {
                None
            } else {
                Some(session_dir)
            };
            let mut cards = self.cards.lock().await;
            cards.insert(
                session_id.clone(),
                crate::bridge::streaming::CardSession::new(acc, new_card_id),
            );
        }

        // Incremental renderer: shows reasoning / tool calls / text as parts
        // complete while the synchronous prompt is still running.
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let render_core = Arc::clone(&self.core);
        let render_sid = session_id.clone();
        let render_done = std::sync::Arc::clone(&done);
        let render_task = tokio::spawn(async move {
            render_poll_loop(&render_core, render_sid, epoch_ms, render_done).await;
        });

        let mut prompt_resp = self
            .opencode
            .prompt(
                &session_id,
                &text,
                &image_inputs(&images),
                self.session_model_override(&session_id).await.as_ref(),
                self.session_agent_override(&session_id).await.as_deref(),
            )
            .await;

        // The mapped session may not exist on the current server — e.g. it was
        // created in an old, now-abandoned store. Clear the mapping, create a
        // fresh session and retry once.
        if prompt_resp.as_ref().is_err_and(|e| e.is_session_not_found()) {
            tracing::warn!("session {} not found on the server; recreating", session_id);
            done.store(true, std::sync::atomic::Ordering::SeqCst);
            let _ = render_task.await;

            // Remove the dead mapping and create a FRESH session on the current
            // server — never fall through to another stale mapping for the
            // thread. Reuse the dead session's directory so the user keeps
            // working in the same project.
            let old_dir = {
                let mut store = self.sessions.lock().await;
                let dir = store.remove(&session_id).map(|e| e.directory);
                store.persist()?;
                dir
            };
            let directory = old_dir
                .filter(|d| !d.is_empty())
                .unwrap_or_else(|| self.default_session_directory());
            let fresh_id = self.create_fresh_session(&thread_key, &text, directory).await?;
            // Re-key the session's live card (accumulator + card identity in
            // one CardSession) from the dead session to the new one — a single
            // remap instead of three maps kept in lockstep.
            {
                let mut cards = self.cards.lock().await;
                if let Some(card) = cards.remove(&session_id) {
                    cards.insert(fresh_id.clone(), card);
                }
            }
            {
                let mut inflight = self.inflight.lock().await;
                inflight.remove(&session_id);
                inflight.insert(fresh_id.clone());
            }
            session_id = fresh_id;

            let done2 = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let render_core2 = Arc::clone(&self.core);
            let render_sid2 = session_id.clone();
            let render_done2 = std::sync::Arc::clone(&done2);
            let render_task2 = tokio::spawn(async move {
                render_poll_loop(&render_core2, render_sid2, epoch_ms, render_done2).await;
            });
            prompt_resp = self
                .opencode
                .prompt(
                    &session_id,
                    &text,
                    &image_inputs(&images),
                    self.session_model_override(&session_id).await.as_ref(),
                    self.session_agent_override(&session_id).await.as_deref(),
                )
                .await;
            done2.store(true, std::sync::atomic::Ordering::SeqCst);
            let _ = render_task2.await;
        } else {
            done.store(true, std::sync::atomic::Ordering::SeqCst);
            let _ = render_task.await;
        }

        let prompt_err = match &prompt_resp {
            Ok(r) => r.error.clone(),
            Err(e) => Some(e.to_string()),
        };

        // Reconcile: render any parts the incremental poll missed, then mark the
        // card Done (or Error). Fall back to the response parts if the fetch fails.
        let final_msgs = self.opencode.messages(&session_id).await.ok();
        {
            let mut cards = self.cards.lock().await;
            if let Some(acc) = cards.get_mut(&session_id).map(|c| &mut c.acc) {
                if let Ok(resp) = &prompt_resp {
                    let mut rendered = false;
                    if let Some(msgs) = &final_msgs {
                        rendered = render_new_turn_parts(acc, msgs, epoch_ms);
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
        // Compute the context-usage ratio for the card footer (input tokens ÷
        // the model's context window), then flush so the footer is on the card.
        if prompt_err.is_none() {
            let info = {
                let cards = self.cards.lock().await;
                cards.get(&session_id).map(|c| &c.acc).and_then(|a| {
                    match (&a.provider_id, &a.model_id, a.context_tokens) {
                        (Some(p), Some(m), input) if input > 0 => Some((p.clone(), m.clone(), input)),
                        _ => None,
                    }
                })
            };
            if let Some((provider, model, input)) = info
                && let Ok(Some(window)) = self.opencode.model_context_window(&provider, &model).await
                && window > 0
            {
                let ratio = (input as f64 / window as f64).clamp(0.0, 1.0);
                let mut cards = self.cards.lock().await;
                if let Some(acc) = cards.get_mut(&session_id).map(|c| &mut c.acc) {
                    acc.context_ratio = Some(ratio);
                }
            }
        }
        flush_card(&self.core, &session_id).await;

        // Baseline for the external-message poller: the newest user message cola
        // itself created (or the submit epoch). Anything newer than this later
        // is from another shared-store client (e.g. OpenChamber).
        let baseline = final_msgs
            .as_ref()
            .map(|msgs| {
                msgs.iter()
                    .filter(|m| m.info.role.as_deref() == Some("user"))
                    .filter_map(|m| m.info.time.as_ref().map(|t| t.created))
                    .max()
                    .unwrap_or(epoch_ms)
            })
            .unwrap_or(epoch_ms)
            .max(epoch_ms);
        self.external.record_prompt_baseline(&session_id, baseline).await;

        // Group completion notice: the streaming card is patched in place, which
        // pushes no new notification — so reply to the requester's message so
        // Feishu notifies them. p2p chats don't need it (the reply lands in the
        // conversation directly).
        if self.group_completion_notice {
            let notice = {
                let cards = self.cards.lock().await;
                cards.get(&session_id).map(|c| &c.acc).and_then(|a| {
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
                let name = self.feishu.user_name(&requester).await.unwrap_or(None);
                if let Err(e) = self
                    .feishu
                    .reply_completion_notice(&reply_to, &requester, name.as_deref(), text)
                    .await
                {
                    tracing::warn!("group completion notice: {}", e);
                }
            }
        }

        {
            let mut inflight = self.inflight.lock().await;
            inflight.remove(&session_id);
        }

        // Permissions are handled by the independent poller spawned in App::run,
        // so a prompt blocked on a permission still gets its card shown.
        Ok(())
    }

    async fn get_or_create_session(
        &self,
        thread_key: &ThreadKey,
        text: &str,
    ) -> crate::error::Result<(String, bool)> {
        if let Some(id) = self.get_session_id(thread_key).await {
            return Ok((id, false));
        }
        let directory = self.default_session_directory();
        let id = self.create_fresh_session(thread_key, text, directory).await?;
        Ok((id, true))
    }

    /// Create a brand-new session on the current server and make it the active
    /// one for the thread. Used when a mapped session no longer exists (404).
    async fn create_fresh_session(
        &self,
        thread_key: &ThreadKey,
        _text: &str,
        directory: String,
    ) -> crate::error::Result<String> {
        let session = self
            .opencode
            .create_session(&self.opencode.new_session_input(Some(&directory)))
            .await?;
        let entry = SessionEntry {
            thread_key: thread_key.clone(),
            session_id: session.id.clone(),
            directory,
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: None,
        };
        let mut store = self.sessions.lock().await;
        store.set_active(entry);
        store.persist()?;
        self.invalidate_session_list_cache().await;
        Ok(session.id)
    }

    /// Handle a card action (permission Allow/Deny, question answer/reject,
    /// error-card retry). Returns the updated card showing the decision, so the
    /// caller can send it back in the ack, plus an optional Toast for instant
    /// client feedback. Dispatches to the flow that owns the action tag.
    pub async fn handle_card_action(self: &Arc<Self>, value: serde_json::Value) -> Option<CardActionResult> {
        let action = value.get("action").and_then(|v| v.as_str()).unwrap_or("");
        match action {
            "perm" => self.permission.handle_card_action(&self.core, &value).await,
            "question" => self.question.handle_card_action(&self.core, &value).await,
            "retry" => self.handle_retry_action(&value).await,
            "switch" => self.handle_switch_card_action(&self.core, &value).await,
            "agent" => self.handle_agent_card_action(&self.core, &value).await,
            "model" => self.handle_model_card_action(&self.core, &value).await,
            "autoaccept" => self.handle_autoaccept_card_action(&self.core, &value).await,
            "help" => self.handle_help_card_action(&self.core, &value).await,
            _ => None,
        }
    }

    /// Handle a `/switch` card button (ADR-0012, issue 04): adopt a session,
    /// create a new one, or re-search. Returns the refreshed card so the ack
    /// patches the card in place, plus a Toast for instant feedback.
    async fn handle_switch_card_action(
        self: &Arc<Self>,
        core: &Arc<SharedCore>,
        value: &serde_json::Value,
    ) -> Option<CardActionResult> {
        let op = value.get("op").and_then(|v| v.as_str()).unwrap_or("");
        let thread_key = thread_key_from_value(value);
        let session_id = value.get("session_id").and_then(|v| v.as_str()).unwrap_or("");
        let keyword = value
            .get("keyword")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        match op {
            "adopt" => {
                // Adopt/switch the target session into this thread. Unlike the
                // text `/switch <id>`, the card carries no `--force`: a session
                // owned by another thread is rejected with a Toast (the owner
                // chat is shown for the user to resolve).
                let sessions = core.cached_session_list().await.ok()?;
                let target = sessions.iter().find(|s| s.id == session_id)?.clone();
                let already_active = {
                    let store = core.sessions.lock().await;
                    store
                        .get_active(&thread_key)
                        .map(|e| e.session_id == target.id)
                        .unwrap_or(false)
                };
                if already_active {
                    return Some(CardActionResult {
                        card: Some(self.build_switch_card_for(core, &thread_key, &keyword).await),
                        toast: Some("已在当前会话".to_string()),
                    });
                }
                // Mapped to another thread → reject (no --force from the card).
                let owner = {
                    let store = core.sessions.lock().await;
                    store.thread_for_session(&target.id)
                };
                if let Some(owner_key) = owner
                    && owner_key != thread_key
                {
                    let chat_name = core
                        .feishu
                        .chat_name(&owner_key.chat_id)
                        .await
                        .unwrap_or(None)
                        .unwrap_or_else(|| owner_key.chat_id.clone());
                    return Some(CardActionResult {
                        card: None,
                        toast: Some(format!(
                            "该会话被 {} 占用，请先请对方解除，或用 `/switch <ID> --force`",
                            chat_name
                        )),
                    });
                }
                let entry = crate::config::SessionEntry {
                    thread_key: thread_key.clone(),
                    session_id: target.id.clone(),
                    directory: target.directory.clone(),
                    agent: target.agent.clone(),
                    model: None,
                    auto_accept: false,
                    topic_anchor: None,
                };
                {
                    let mut store = core.sessions.lock().await;
                    store.set_active(entry);
                    if let Err(e) = store.persist() {
                        tracing::warn!("switch card adopt: persist failed: {}", e);
                    }
                }
                core.invalidate_session_list_cache().await;
                let toast = format!("已接管「{}」", crate::bridge::command::title_or_id_tail(&target));
                Some(CardActionResult {
                    card: Some(self.build_switch_card_for(core, &thread_key, &keyword).await),
                    toast: Some(toast),
                })
            }
            "new" => {
                // Fresh session in the current project (equivalent to `/new`).
                let directory = {
                    let store = core.sessions.lock().await;
                    store
                        .get_active(&thread_key)
                        .map(|e| e.directory.clone())
                        .filter(|d| !d.is_empty())
                        .unwrap_or_else(|| core.default_session_directory())
                };
                match core
                    .opencode
                    .create_session(&core.opencode.new_session_input(Some(&directory)))
                    .await
                {
                    Ok(session) => {
                        let entry = crate::config::SessionEntry {
                            thread_key: thread_key.clone(),
                            session_id: session.id.clone(),
                            directory,
                            agent: None,
                            model: None,
                            auto_accept: false,
                            topic_anchor: None,
                        };
                        {
                            let mut store = core.sessions.lock().await;
                            store.set_active(entry);
                            if let Err(e) = store.persist() {
                                tracing::warn!("switch card new: persist failed: {}", e);
                            }
                        }
                        core.invalidate_session_list_cache().await;
                        Some(CardActionResult {
                            card: Some(self.build_switch_card_for(core, &thread_key, "").await),
                            toast: Some("已新建会话".to_string()),
                        })
                    }
                    Err(e) => {
                        tracing::warn!("switch card new session failed: {}", e);
                        None
                    }
                }
            }
            "search" => {
                // A search is an explicit refresh request: drop the session-list
                // cache so the re-filter sees newly created/adopted sessions.
                core.invalidate_session_list_cache().await;
                Some(CardActionResult {
                    card: Some(self.build_switch_card_for(core, &thread_key, &keyword).await),
                    toast: None,
                })
            }
            "topic_adopt" => {
                // "建话题接管" (ADR-0016): open a NEW Feishu topic around an
                // existing session, in one gesture from the session card. The
                // topic anchors on the card's own message (`open_message_id`,
                // threaded through by `extract_card_action_value`), so fallback
                // cards can reply inside it. No `--force` from the card: a
                // session owned by another thread is rejected with a Toast.
                let Some(open_message_id) = value.get("open_message_id").and_then(|v| v.as_str()) else {
                    return Some(CardActionResult {
                        card: None,
                        toast: Some("无法创建话题（缺少卡片消息引用）".to_string()),
                    });
                };
                let open_message_id = open_message_id.to_string();
                let sessions = core.cached_session_list().await.ok()?;
                let target = sessions.iter().find(|s| s.id == session_id)?.clone();
                if target.is_child() {
                    return Some(CardActionResult {
                        card: None,
                        toast: Some("子任务会话不支持接管".to_string()),
                    });
                }
                let owner = {
                    let store = core.sessions.lock().await;
                    store.thread_for_session(&target.id)
                };
                if let Some(owner_key) = owner
                    && owner_key != thread_key
                {
                    let chat_name = core
                        .feishu
                        .chat_name(&owner_key.chat_id)
                        .await
                        .unwrap_or(None)
                        .unwrap_or_else(|| owner_key.chat_id.clone());
                    return Some(CardActionResult {
                        card: None,
                        toast: Some(format!(
                            "会话被 {} 占用，请用 `/topic --adopt <ID> --force`",
                            chat_name
                        )),
                    });
                }
                // Create a topic anchored on the card message and map the
                // adopted session to the new topic's ThreadKey (shared with the
                // text `/topic --adopt` form, ADR-0016).
                let new_thread_id = match crate::bridge::command::create_topic_and_map_adopted(
                    core,
                    &thread_key,
                    &target,
                    &open_message_id,
                )
                .await
                {
                    Ok(Some(id)) => id,
                    Ok(None) => {
                        return Some(CardActionResult {
                            card: None,
                            toast: Some("创建话题失败（未返回 thread_id）".to_string()),
                        });
                    }
                    Err(e) => {
                        tracing::warn!("switch card topic_adopt: create topic failed: {}", e);
                        return Some(CardActionResult {
                            card: None,
                            toast: Some("创建话题失败".to_string()),
                        });
                    }
                };
                tracing::info!(
                    "switch card topic_adopt: created topic {} for session {} in chat {}",
                    new_thread_id,
                    target.id,
                    thread_key.chat_id
                );
                Some(CardActionResult {
                    card: Some(self.build_switch_card_for(core, &thread_key, &keyword).await),
                    toast: Some("已建话题接管".to_string()),
                })
            }
            _ => None,
        }
    }

    /// Rebuild the `/switch` card for a thread with the given search keyword.
    async fn build_switch_card_for(
        self: &Arc<Self>,
        core: &Arc<SharedCore>,
        thread_key: &ThreadKey,
        keyword: &str,
    ) -> serde_json::Value {
        let (shown, active_id, mapped_ids) =
            crate::bridge::command::switch_card_data(core, thread_key, keyword).await;
        crate::feishu::card::build_switch_card(thread_key, &shown, keyword, active_id.as_deref(), &mapped_ids)
    }

    /// Handle an `/agent` picker-card button: record the per-session override
    /// and refresh the card.
    async fn handle_agent_card_action(
        self: &Arc<Self>,
        core: &Arc<SharedCore>,
        value: &serde_json::Value,
    ) -> Option<CardActionResult> {
        let agent = value
            .get("value")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let thread_key = thread_key_from_value(value);
        let Some(mut entry) = core.sessions.lock().await.get_active(&thread_key).cloned() else {
            return Some(CardActionResult {
                card: None,
                toast: Some("当前对话还没有会话".to_string()),
            });
        };
        entry.agent = Some(agent.clone());
        {
            let mut store = core.sessions.lock().await;
            store.set_active(entry);
            if let Err(e) = store.persist() {
                tracing::warn!("agent card: persist failed: {}", e);
            }
        }
        let agents = core.opencode.list_agents().await;
        Some(CardActionResult {
            card: Some(crate::feishu::card::build_agent_card(&thread_key, &agents)),
            toast: Some(format!("Agent: {agent}（下一条消息开始生效）")),
        })
    }

    /// Handle a `/model` picker-card button. Two levels (the provider → model
    /// flow): a `level: "provider"` button either opens a provider's model list
    /// or (`value == PICKER_BACK_TO_PROVIDERS`) returns to the full provider
    /// list; a `level: "model"` button records the per-session override.
    async fn handle_model_card_action(
        self: &Arc<Self>,
        core: &Arc<SharedCore>,
        value: &serde_json::Value,
    ) -> Option<CardActionResult> {
        let thread_key = thread_key_from_value(value);
        let picked = value
            .get("value")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        // Level 1: provider navigation — build and swap in the next picker page.
        if value.get("level").and_then(|v| v.as_str())
            == Some(crate::feishu::card::PickerLevel::Provider.as_str())
        {
            let providers = core.opencode.list_models().await;
            let cards = if picked == crate::feishu::card::PICKER_BACK_TO_PROVIDERS {
                crate::feishu::card::build_model_provider_cards(&thread_key, &providers)
            } else {
                let models: Vec<String> = providers
                    .iter()
                    .find(|p| p.provider == picked)
                    .map(|p| p.models.clone())
                    .unwrap_or_default();
                crate::feishu::card::build_model_picker_cards(&thread_key, &picked, &models)
            };
            return Some(Self::picker_pages_result(core, &thread_key, cards).await);
        }

        // Level 2: a concrete model — record the override.
        if crate::opencode::client::parse_model(&picked).is_none() {
            return Some(CardActionResult {
                card: None,
                toast: Some("模型格式应为 <provider>/<model>".to_string()),
            });
        }
        let Some(mut entry) = core.sessions.lock().await.get_active(&thread_key).cloned() else {
            return Some(CardActionResult {
                card: None,
                toast: Some("当前对话还没有会话".to_string()),
            });
        };
        entry.model = Some(picked.clone());
        {
            let mut store = core.sessions.lock().await;
            store.set_active(entry);
            if let Err(e) = store.persist() {
                tracing::warn!("model card: persist failed: {}", e);
            }
        }
        Some(CardActionResult {
            card: None,
            toast: Some(format!("Model: {picked}（下一条消息开始生效）")),
        })
    }

    /// Turn a page of picker cards into a card-action result: the FIRST card
    /// replaces the clicked one in place (via the ack); the remaining pages are
    /// posted into the chat so the full picker stays visible.
    async fn picker_pages_result(
        core: &Arc<SharedCore>,
        thread_key: &ThreadKey,
        cards: Vec<serde_json::Value>,
    ) -> CardActionResult {
        let (first, rest) = cards.split_first().map_or((None, &[][..]), |(f, r)| (Some(f), r));
        for card in rest {
            let _ = core.feishu.send_card("chat_id", &thread_key.chat_id, card).await;
        }
        CardActionResult {
            card: first.cloned(),
            toast: None,
        }
    }

    /// Handle an `/autoaccept` toggle-card button: switch the flag and refresh
    /// the card.
    async fn handle_autoaccept_card_action(
        self: &Arc<Self>,
        core: &Arc<SharedCore>,
        value: &serde_json::Value,
    ) -> Option<CardActionResult> {
        let on = value.get("value").and_then(|v| v.as_str()) == Some("on");
        let thread_key = thread_key_from_value(value);
        let current_entry = {
            let store = core.sessions.lock().await;
            store.get_active(&thread_key).cloned()
        };
        if on && let Some(e) = &current_entry {
            core.approve_pending_for_session(&e.session_id, &e.directory)
                .await;
        }
        if let Some(mut e) = current_entry {
            e.auto_accept = on;
            let mut store = core.sessions.lock().await;
            store.set_active(e);
            if let Err(e) = store.persist() {
                tracing::warn!("autoaccept card: persist failed: {}", e);
            }
        }
        let current_on = core
            .sessions
            .lock()
            .await
            .get_active(&thread_key)
            .map(|e| e.auto_accept)
            .unwrap_or(false);
        Some(CardActionResult {
            card: Some(crate::feishu::card::build_autoaccept_card(
                &thread_key,
                current_on,
            )),
            toast: Some(if on {
                "已开启自动审批".to_string()
            } else {
                "已关闭自动审批".to_string()
            }),
        })
    }

    /// Handle a `/help` navigation-card button: re-run the command text in the
    /// same thread (the card is patched to a brief confirmation; the command's
    /// own reply/card follows).
    async fn handle_help_card_action(
        self: &Arc<Self>,
        core: &Arc<SharedCore>,
        value: &serde_json::Value,
    ) -> Option<CardActionResult> {
        let cmd = value.get("cmd").and_then(|v| v.as_str()).unwrap_or("");
        let thread_key = thread_key_from_value(value);
        if cmd.is_empty() {
            return None;
        }
        // Re-run the command text through the normal message pipeline (it has
        // no message_id to reply to, so pass an empty anchor — the command's
        // output goes to the thread top-level).
        let text = if cmd.starts_with('/') {
            cmd.to_string()
        } else {
            format!("/{cmd}")
        };
        let incoming = crate::bridge::IncomingMessage {
            message_id: String::new(),
            chat_id: thread_key.chat_id.clone(),
            chat_type: "p2p".to_string(),
            thread_id: if thread_key.thread_id == thread_key.chat_id {
                None
            } else {
                Some(thread_key.thread_id.clone())
            },
            parent_id: None,
            text,
            images: Vec::new(),
            requester_open_id: None,
        };
        core.invalidate_session_list_cache().await;
        let app = Arc::clone(self);
        tokio::spawn(async move {
            app.handle_message(incoming).await;
        });
        Some(CardActionResult {
            card: Some(crate::feishu::card::build_help_card(&thread_key)),
            toast: Some("已执行".to_string()),
        })
    }

    /// Re-submit a failed prompt on the SAME card (error-card "retry" button).
    /// The card callback must ack within 3s, so spawn the prompt pipeline and
    /// return a "retrying" card immediately; run_prompt then resets the card to
    /// Loading and streams the new attempt into it.
    async fn handle_retry_action(self: &Arc<Self>, value: &serde_json::Value) -> Option<CardActionResult> {
        let sid = value
            .get("session_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_default();
        if sid.is_empty() {
            return None;
        }
        let inflight = { self.inflight.lock().await.contains(&sid) };
        let ctx = {
            let cards = self.cards.lock().await;
            cards.get(&sid).map(|c| {
                (
                    c.acc.prompt.clone().unwrap_or_default(),
                    c.acc.reply_to_message_id.clone().unwrap_or_default(),
                    c.acc.title.clone(),
                    c.acc.requester_open_id.clone(),
                    c.acc.is_group,
                )
            })
        };
        let card_id = {
            let cards = self.cards.lock().await;
            cards.get(&sid).and_then(|c| c.card_message_id.clone())
        };
        let thread_key = self.sessions.lock().await.thread_for_session(&sid);
        if !inflight
            && let Some((text, reply_to, subtitle, requester, is_group)) = ctx
            && !text.is_empty()
            && let Some(card_id) = card_id
            && let Some(thread_key) = thread_key
        {
            let app = Arc::clone(self);
            tokio::spawn(async move {
                if let Err(e) = app
                    .run_prompt(PromptContext {
                        session_id: sid.to_string(),
                        thread_key,
                        text,
                        message_id: reply_to,
                        subtitle,
                        existing_card_id: Some(card_id),
                        requester_open_id: requester,
                        is_group,
                        images: Vec::new(),
                    })
                    .await
                {
                    tracing::error!("retry prompt: {}", e);
                }
            });
            let mut r = crate::bridge::pollers::result_card("⏳ 正在重试...", "blue", "已重新提交原始问题。");
            r.toast = Some("正在重试...".to_string());
            Some(r)
        } else {
            // Nothing to retry (no stored prompt / card, or a prompt is already
            // in flight): keep the card as it is.
            tracing::warn!(
                "retry: no retryable context for session {} (inflight={})",
                sid,
                inflight
            );
            None
        }
    }
}
