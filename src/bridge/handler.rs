use std::ops::Deref;
use std::sync::Arc;

use crate::bridge::command;
use crate::bridge::core::{SessionListCache, SharedCore};
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
}

/// The bridge coordinator. Owns the state shared by every flow ([`SharedCore`])
/// plus the per-flow modules that hold their own private state. `Deref`s to the
/// shared core so flows and callers can reach `app.sessions`, `app.opencode`,
/// etc. without threading a separate handle.
pub struct App {
    core: Arc<SharedCore>,
    /// Weak self-reference, set once inside `run` (which holds the Arc). Lets
    /// the `EventSink` trait impl (which only has `&self`) recover a
    /// `&Arc<App>` to hand to the inherent methods. `Weak` so it never keeps
    /// the app alive — no reference cycle; `OnceLock` because it is written
    /// exactly once, before any event can arrive.
    self_weak: std::sync::OnceLock<std::sync::Weak<App>>,
    /// Permission flow: owns `sent_permission_cards`, polls pending requests,
    /// and handles the "perm" card action.
    pub permission: super::permission::PermissionFlow,
    /// Question flow: owns `question_requests` / `question_partial` /
    /// `sent_question_cards`, polls pending questions, and handles the
    /// "question" card action (answer / submit / reject).
    pub question: super::question::QuestionFlow,
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
    async fn handle_message(
        &self,
        message_id: String,
        chat_id: String,
        chat_type: String,
        thread_id: Option<String>,
        text: String,
        requester_open_id: Option<String>,
    ) {
        if let Some(app) = self.self_arc() {
            app.handle_message(message_id, chat_id, chat_type, thread_id, text, requester_open_id)
                .await;
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

impl App {
    pub fn new(
        cfg: Config,
        opencode: Arc<dyn opencode::Backend>,
        feishu: Arc<dyn feishu::Platform>,
    ) -> anyhow::Result<Self> {
        let core = Arc::new(SharedCore::new(&cfg, opencode, feishu)?);
        Ok(Self {
            self_weak: std::sync::OnceLock::new(),
            permission: super::permission::PermissionFlow::new(),
            question: super::question::QuestionFlow::new(),
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
        // After a `/restart`, announce it in the chat that requested it.
        let notify_path = command::restart_notify_path();
        if let Ok(raw) = std::fs::read_to_string(&notify_path)
            && let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw)
            && let Some(chat_id) = v.get("chat_id").and_then(|c| c.as_str())
        {
            let _ = std::fs::remove_file(&notify_path);
            let card = serde_json::json!({
                "schema": "2.0",
                "config": { "wide_screen_mode": true },
                "header": { "title": { "tag": "plain_text", "content": "♻️ 已重启" }, "template": "green" },
                "body": { "elements": [ { "tag": "markdown", "content": "cola 已重启完成。" } ] }
            });
            match self.feishu.send_card("chat_id", chat_id, &card).await {
                Ok(_) => tracing::info!("announced restart in chat {}", chat_id),
                Err(e) => tracing::warn!("restart announce failed: {}", e),
            }
        }

        // Discover cola's own open_id so @mentions of the bot can be recognised
        // and stripped from prompt text (Feishu delivers them as `@_user_N`).
        // Owned by the ws module now (see `feishu::ws`).

        let ws = Arc::clone(&self);
        let ws_feishu = Arc::clone(&self.feishu);
        let ws_state = Arc::new(feishu::ws::WsState::new());
        let perm = Arc::clone(&self);
        let question = Arc::clone(&self);
        let external = Arc::clone(&self);
        let reconnect = Arc::clone(&self);
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
            if let Err(e) = perm.permission.poll_loop(&perm).await {
                tracing::error!("Permission poller: {}", e);
            }
        });
        // Questions (the interactive `question` tool) work the same way: the AI
        // blocks until answered, the event never reaches the global SSE, so poll
        // and surface them as Feishu cards.
        let question_task = tokio::spawn(async move {
            if let Err(e) = question.question.poll_loop(&question).await {
                tracing::error!("Question poller: {}", e);
            }
        });
        // Notify Feishu when someone posts a message from another shared-store
        // client (e.g. OpenChamber) while cola is idle on that session.
        let external_task = tokio::spawn(async move {
            if let Err(e) = external.external.poll_loop(&external).await {
                tracing::error!("External message poller: {}", e);
            }
        });
        // The OpenCode server cola attaches to is managed by another tool that
        // can restart it (new pid/port/password). Re-detect a changed server so
        // cola reconnects instead of 502ing against the dead port forever.
        let reconnect_task = tokio::spawn(async move {
            if let Err(e) = crate::bridge::pollers::reconnect_poll_loop(&reconnect).await {
                tracing::error!("Reconnect poller: {}", e);
            }
        });
        tokio::try_join!(ws_task, perm_task, question_task, external_task, reconnect_task)?;
        Ok(())
    }

    pub async fn handle_message(
        self: &Arc<Self>,
        message_id: String,
        chat_id: String,
        chat_type: String,
        thread_id: Option<String>,
        text: String,
        requester_open_id: Option<String>,
    ) {
        let kind = ConversationKind::classify(&chat_type, thread_id.as_deref());
        let thread_key = kind.thread_key(&chat_id, thread_id.as_deref());
        if let Some(cmd) = command::parse_command(&text) {
            if let Err(e) = self.handle_command(cmd, thread_key, &message_id, kind).await {
                tracing::error!("Cmd: {}", e);
            }
            return;
        }
        let is_group = chat_type == "group";
        if let Err(e) = self
            .handle_prompt(thread_key, text, &message_id, kind, requester_open_id, is_group)
            .await
        {
            tracing::error!("Prompt: {}", e);
        }
    }

    pub(crate) async fn handle_prompt(
        self: &Arc<Self>,
        thread_key: ThreadKey,
        text: String,
        message_id: &str,
        kind: ConversationKind,
        requester_open_id: Option<String>,
        is_group: bool,
    ) -> crate::error::Result<()> {
        let (session_id, created) = self.get_or_create_session(&thread_key, &text).await?;

        // First message on a group's top level created a lobby session: reply
        // once with guidance so the user knows each topic isolates a session.
        if created && kind == ConversationKind::GroupLobby {
            self.feishu.reply_text(message_id, GROUP_LOBBY_GUIDANCE).await?;
        }

        let subtitle = self.session_subtitle(&thread_key, &text).await;

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
                match self.opencode.prompt_async(&session_id, &text).await {
                    Ok(()) => {
                        tracing::info!(
                            "supplement: session {} in-flight, message queued to merge into current turn",
                            session_id
                        );
                        if kind == ConversationKind::P2p || kind == ConversationKind::Topic {
                            let _ = self
                                .feishu
                                .reply_text(
                                    message_id,
                                    "📨 已收到补充，将并入当前处理。若当前轮已结束，会作为下一条消息继续。",
                                )
                                .await;
                        }
                    }
                    Err(e) => {
                        tracing::warn!("supplement: prompt_async failed: {}", e);
                        let _ = self
                            .feishu
                            .reply_text(message_id, "⚠️ 补充消息发送失败，请稍后重试。")
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
            message_id: message_id.to_string(),
            subtitle,
            existing_card_id: None,
            requester_open_id,
            is_group,
        })
        .await
    }

    /// The session/thread name shown as the card subtitle, formatted as
    /// `<title> · <id-tail>` (e.g. "你好 · 01ba0ed"). The OpenCode server's OWN
    /// session title (what OpenChamber shows) is the single source of truth
    /// (ADR-0007) — fetched on demand, never cached locally. While the server
    /// still has the default `New session - ...` title (or the title is empty),
    /// the id-tail alone identifies the session; the current prompt is never
    /// echoed (the reply context already shows it).
    async fn session_subtitle(&self, thread_key: &ThreadKey, text: &str) -> String {
        let prompt_preview: String = text.chars().take(50).collect();
        let session_id = {
            let store = self.sessions.lock().await;
            let Some(entry) = store.get_active(thread_key) else {
                return String::new();
            };
            entry.session_id.clone()
        };
        let mut name = String::new();
        if let Ok(info) = self.opencode.session_info(&session_id, None).await
            && let Some(t) = info.title.filter(|t| !t.is_empty())
        {
            name = crate::feishu::card::clean_session_label(&t);
        }
        let id_tail: String = session_id
            .strip_prefix("ses_")
            .unwrap_or(&session_id)
            .chars()
            .take(7)
            .collect();
        if name.is_empty() || name == prompt_preview {
            // No echo of the current prompt, but still identify the session.
            id_tail
        } else {
            format!("{} · {}", name, id_tail)
        }
    }

    /// Refresh the streaming card's subtitle from the server's live session
    /// title, returning true if it changed (and the card was re-flushed).
    ///
    /// OpenCode/OpenChamber automatically summarize and rename a session after a
    /// turn; cola's card subtitle is only captured when the prompt starts, so it
    /// would otherwise stay on the "new session" default title until restart.
    /// Called periodically from the render poll loop, so the title follows the
    /// server within a poll interval.
    pub(crate) async fn refresh_session_title(self: &Arc<Self>, session_id: &str) -> bool {
        // Only meaningful while a turn is actively streaming on a live card.
        let acc_present = self.accumulators.lock().await.contains_key(session_id);
        if !acc_present {
            return false;
        }
        let thread_key = self.sessions.lock().await.thread_for_session(session_id);
        let Some(thread_key) = thread_key else {
            return false;
        };
        // The subtitle is formatted with the session id-tail; recompute it now
        // and keep whatever the server currently reports.
        let fresh = self.session_subtitle(&thread_key, "").await;
        if fresh.is_empty() {
            return false;
        }
        let mut accs = self.accumulators.lock().await;
        let Some(acc) = accs.get_mut(session_id) else {
            return false;
        };
        if acc.title == fresh {
            return false;
        }
        tracing::info!(
            "session {} title updated: {:?} -> {:?}",
            session_id,
            acc.title,
            fresh
        );
        acc.title = fresh;
        drop(accs);
        crate::bridge::render::flush_card(self, session_id).await;
        true
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
        if let Some(cid) = existing_card_id {
            // Retry: reset the SAME card to Loading instead of replying a new one.
            if let Err(e) = self.feishu.update_message(&cid, &loading).await {
                tracing::warn!("retry: reset card failed: {}", e);
            }
        } else {
            let cid = self.feishu.reply_card(&message_id, &loading).await?;
            let mut ids = self.card_message_ids.lock().await;
            ids.insert(session_id.clone(), cid);
        }
        let epoch_ms = chrono::Utc::now().timestamp_millis();
        {
            // Fresh accumulator per prompt: reuse leaks stale text/tools from the
            // previous turn into the next card.
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
            let mut accs = self.accumulators.lock().await;
            accs.insert(session_id.clone(), acc);
        }

        // Incremental renderer: shows reasoning / tool calls / text as parts
        // complete while the synchronous prompt is still running.
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let render_app = Arc::clone(self);
        let render_sid = session_id.clone();
        let render_done = std::sync::Arc::clone(&done);
        let render_task = tokio::spawn(async move {
            render_poll_loop(&render_app, render_sid, epoch_ms, render_done).await;
        });

        let mut prompt_resp = self.opencode.prompt(&session_id, &text).await;

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
            // Re-key the card id + accumulator from the dead session to the new one.
            {
                let mut ids = self.card_message_ids.lock().await;
                if let Some(cid) = ids.remove(&session_id) {
                    ids.insert(fresh_id.clone(), cid);
                }
            }
            {
                let mut accs = self.accumulators.lock().await;
                if let Some(acc) = accs.remove(&session_id) {
                    accs.insert(fresh_id.clone(), acc);
                }
            }
            {
                let mut inflight = self.inflight.lock().await;
                inflight.remove(&session_id);
                inflight.insert(fresh_id.clone());
            }
            session_id = fresh_id;

            let done2 = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let render_app2 = Arc::clone(self);
            let render_sid2 = session_id.clone();
            let render_done2 = std::sync::Arc::clone(&done2);
            let render_task2 = tokio::spawn(async move {
                render_poll_loop(&render_app2, render_sid2, epoch_ms, render_done2).await;
            });
            prompt_resp = self.opencode.prompt(&session_id, &text).await;
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
            let mut accs = self.accumulators.lock().await;
            if let Some(acc) = accs.get_mut(&session_id) {
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
                let accs = self.accumulators.lock().await;
                accs.get(&session_id)
                    .and_then(|a| match (&a.provider_id, &a.model_id, a.context_tokens) {
                        (Some(p), Some(m), input) if input > 0 => Some((p.clone(), m.clone(), input)),
                        _ => None,
                    })
            };
            if let Some((provider, model, input)) = info
                && let Ok(Some(window)) = self.opencode.model_context_window(&provider, &model).await
                && window > 0
            {
                let ratio = (input as f64 / window as f64).clamp(0.0, 1.0);
                let mut accs = self.accumulators.lock().await;
                if let Some(acc) = accs.get_mut(&session_id) {
                    acc.context_ratio = Some(ratio);
                }
            }
        }
        flush_card(self, &session_id).await;

        // Baseline for the external-message poller: the newest user message cola
        // itself created (or the submit epoch). Anything newer than this later
        // is from another shared-store client (e.g. OpenChamber).
        {
            let mut map = self.external.last_user_msg_epoch.lock().await;
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
            map.insert(session_id.clone(), baseline);
        }

        // Group completion notice: the streaming card is patched in place, which
        // pushes no new notification — so reply to the requester's message so
        // Feishu notifies them. p2p chats don't need it (the reply lands in the
        // conversation directly).
        if self.group_completion_notice {
            let notice = {
                let accs = self.accumulators.lock().await;
                accs.get(&session_id).and_then(|a| {
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

    pub(crate) async fn get_session_id(&self, thread_key: &ThreadKey) -> Option<String> {
        self.sessions
            .lock()
            .await
            .get_active(thread_key)
            .map(|e| e.session_id.clone())
    }

    /// After `/autoaccept on`: answer every permission request that is ALREADY
    /// pending for `session_id` (or one of its sub-task child sessions) with
    /// "once". The permission poller's `seen` set skips requests it has already
    /// surfaced, so enabling autoaccept would otherwise leave old cards hanging
    /// forever. Returns how many requests were approved.
    pub(crate) async fn approve_pending_for_session(&self, session_id: &str, directory: &str) -> usize {
        let Ok(perms) = self.opencode.list_permissions(Some(directory)).await else {
            return 0;
        };
        let mut approved = 0usize;
        for p in &perms {
            // Match the session itself or a sub-task child (its parent chain).
            let Some(sid) = p.session_id.clone() else { continue };
            if sid != session_id && !self.session_descends_from(&sid, session_id, directory).await {
                continue;
            }
            match self
                .opencode
                .reply_permission(&p.request_id, "once", Some(directory))
                .await
            {
                Ok(()) => {
                    tracing::info!(
                        "Auto-accepted pending permission {} on session {} ({})",
                        p.request_id,
                        sid,
                        p.permission.as_deref().unwrap_or("?")
                    );
                    approved += 1;
                }
                Err(e) => tracing::warn!("auto-accept pending {} on session {}: {}", p.request_id, sid, e),
            }
        }
        approved
    }

    /// Whether `candidate` is `root` or a sub-task child reachable by walking
    /// up its parent chain (sub-task child sessions carry their own sessionID).
    async fn session_descends_from(&self, candidate: &str, root: &str, directory: &str) -> bool {
        if candidate == root {
            return true;
        }
        let mut current = candidate.to_string();
        for _ in 0..8 {
            let Ok(info) = self.opencode.session_info(&current, Some(directory)).await else {
                return false;
            };
            let Some(parent) = info.parent_id.filter(|p| p != &current) else {
                return false;
            };
            if parent == root {
                return true;
            }
            current = parent;
        }
        false
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
            auto_accept: false,
            topic_anchor: None,
        };
        let mut store = self.sessions.lock().await;
        store.set_active(entry);
        store.persist()?;
        self.invalidate_session_list_cache().await;
        Ok(session.id)
    }

    /// The current `GET /session` snapshot, fetching (and caching for 30 s) when
    /// missing or stale. Used by `/list`, `/switch` and `/attach` so rapid
    /// reuse stays off the wire.
    pub(crate) async fn cached_session_list(
        &self,
    ) -> crate::error::Result<Vec<crate::opencode::SessionListInfo>> {
        let now = std::time::Instant::now();
        {
            let cache = self.session_list_cache.lock().await;
            if let Some(c) = cache.as_ref()
                && c.fresh()
            {
                return Ok(c.sessions.clone());
            }
        }
        let sessions = self.opencode.list_sessions().await?;
        *self.session_list_cache.lock().await = Some(SessionListCache {
            fetched_at: now,
            sessions: sessions.clone(),
        });
        Ok(sessions)
    }

    /// Drop the `/list` cache. Called whenever cola creates, adopts, forgets or
    /// renames a session, so the next `/list`/`/switch`/`/attach` is fresh.
    pub(crate) async fn invalidate_session_list_cache(&self) {
        *self.session_list_cache.lock().await = None;
    }

    /// Handle a card action (permission Allow/Deny, question answer/reject,
    /// error-card retry). Returns the updated card showing the decision, so the
    /// caller can send it back in the ack, plus an optional Toast for instant
    /// client feedback. Dispatches to the flow that owns the action tag.
    pub async fn handle_card_action(self: &Arc<Self>, value: serde_json::Value) -> Option<CardActionResult> {
        let action = value.get("action").and_then(|v| v.as_str()).unwrap_or("");
        match action {
            "perm" => self.permission.handle_card_action(self, &value).await,
            "question" => self.question.handle_card_action(self, &value).await,
            "retry" => self.handle_retry_action(&value).await,
            _ => None,
        }
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
            let accs = self.accumulators.lock().await;
            accs.get(&sid).map(|a| {
                (
                    a.prompt.clone().unwrap_or_default(),
                    a.reply_to_message_id.clone().unwrap_or_default(),
                    a.title.clone(),
                    a.requester_open_id.clone(),
                    a.is_group,
                )
            })
        };
        let card_id = self.card_message_ids.lock().await.get(&sid).cloned();
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

// ===== Test double support (mock Backend + recording Platform) =====

#[cfg(test)]
mod test_support {
    use super::*;

    /// A recorded `reply_question` call: (request_id, answers).
    type QuestionReplyRecord = (String, Vec<Vec<String>>);

    #[derive(Debug, Clone)]
    #[allow(dead_code)] // the recording adapter captures full call details for assertions
    pub enum PlatformCall {
        ReplyCard {
            reply_to: String,
            card: serde_json::Value,
        },
        SendCard {
            receive_id: String,
            card: serde_json::Value,
        },
        UpdateMessage {
            message_id: String,
            card: serde_json::Value,
        },
        ReplyText {
            message_id: String,
            text: String,
        },
        ReplyInThread {
            message_id: String,
            text: String,
            thread_id: Option<String>,
        },
        CompletionNotice {
            reply_to: String,
            open_id: String,
            name: Option<String>,
            text: String,
        },
    }

    /// Records every card cola would send, instead of posting to Feishu.
    pub struct RecordingPlatform {
        pub calls: Arc<tokio::sync::Mutex<Vec<PlatformCall>>>,
        /// open_id → display name served by `user_name` (empty = lookup fails).
        pub user_names: std::collections::HashMap<String, String>,
        /// chat_id → display name served by `chat_name` (absent = None).
        pub chat_names: std::collections::HashMap<String, String>,
    }

    impl RecordingPlatform {
        pub fn new() -> Self {
            Self {
                calls: Arc::new(tokio::sync::Mutex::new(Vec::new())),
                user_names: std::collections::HashMap::new(),
                chat_names: std::collections::HashMap::new(),
            }
        }
    }

    #[async_trait::async_trait]
    impl feishu::Platform for RecordingPlatform {
        async fn get_ws_endpoint(&self) -> crate::error::Result<String> {
            Ok("wss://example.test".into())
        }

        async fn reply_card(&self, reply_to: &str, card: &serde_json::Value) -> crate::error::Result<String> {
            self.calls.lock().await.push(PlatformCall::ReplyCard {
                reply_to: reply_to.into(),
                card: card.clone(),
            });
            Ok("msg_reply".into())
        }

        async fn send_card(
            &self,
            _receive_id_type: &str,
            receive_id: &str,
            card: &serde_json::Value,
        ) -> crate::error::Result<String> {
            self.calls.lock().await.push(PlatformCall::SendCard {
                receive_id: receive_id.into(),
                card: card.clone(),
            });
            Ok("msg_sent".into())
        }

        async fn update_message(
            &self,
            message_id: &str,
            card: &serde_json::Value,
        ) -> crate::error::Result<()> {
            self.calls.lock().await.push(PlatformCall::UpdateMessage {
                message_id: message_id.into(),
                card: card.clone(),
            });
            Ok(())
        }

        async fn reply_text(&self, message_id: &str, text: &str) -> crate::error::Result<String> {
            self.calls.lock().await.push(PlatformCall::ReplyText {
                message_id: message_id.into(),
                text: text.into(),
            });
            Ok("msg_text".into())
        }

        async fn reply_in_thread(
            &self,
            message_id: &str,
            text: &str,
        ) -> crate::error::Result<(String, Option<String>)> {
            self.calls.lock().await.push(PlatformCall::ReplyInThread {
                message_id: message_id.into(),
                text: text.into(),
                thread_id: Some("omt_created_topic".into()),
            });
            // The mock's created topic-reply message id becomes the anchor.
            Ok(("msg_topic_reply".into(), Some("omt_created_topic".into())))
        }

        async fn reply_completion_notice(
            &self,
            message_id: &str,
            open_id: &str,
            name: Option<&str>,
            text: &str,
        ) -> crate::error::Result<String> {
            self.calls.lock().await.push(PlatformCall::CompletionNotice {
                reply_to: message_id.into(),
                open_id: open_id.into(),
                name: name.map(|s| s.to_string()),
                text: text.into(),
            });
            Ok("msg_notice".into())
        }

        async fn user_name(&self, open_id: &str) -> crate::error::Result<Option<String>> {
            Ok(self.user_names.get(open_id).cloned())
        }

        async fn chat_name(&self, chat_id: &str) -> crate::error::Result<Option<String>> {
            Ok(self.chat_names.get(chat_id).cloned())
        }

        async fn bot_open_id(&self) -> crate::error::Result<String> {
            Ok("ou_test_bot".into())
        }

        async fn list_messages(
            &self,
            _container_id_type: &str,
            _container_id: &str,
        ) -> crate::error::Result<Vec<crate::feishu::client::ChatMessage>> {
            Ok(vec![crate::feishu::client::ChatMessage {
                message_id: "msg_in_topic_anchor".into(),
                msg_type: "interactive".into(),
                create_time: "0".into(),
                chat_id: "chat_1".into(),
                sender: Some(crate::feishu::client::ChatMessageSender {
                    id: Some("cli_bot".into()),
                    sender_type: Some("app".into()),
                }),
                body: None,
            }])
        }
    }

    /// Serves scripted parts/permissions instead of a live OpenCode server.
    pub struct MockBackend {
        pub parts: serde_json::Value,
        pub permissions: Vec<opencode::client::PermissionRequest>,
        /// When set, `messages` returns this as a fresh user message (simulates
        /// a message posted from OpenChamber).
        pub external_user_message: Option<String>,
        /// Records every `reply_permission` call: (request_id, reply).
        pub reply_permission_calls: Arc<tokio::sync::Mutex<Vec<(String, String)>>>,
        /// session_id → server title (simulates OpenChamber's session title).
        /// `std::sync::Mutex` for interior mutability: `update_session_title`
        /// writes it through `&self` (the trait requires `&self`).
        pub session_titles: std::sync::Mutex<std::collections::HashMap<String, String>>,
        /// Pending questions served by `list_questions`.
        pub questions: Vec<opencode::client::QuestionRequest>,
        /// Records `reply_question` calls: (request_id, answers).
        pub reply_question_calls: Arc<tokio::sync::Mutex<Vec<QuestionReplyRecord>>>,
        /// When set, `prompt` fails with this message (simulates a provider 503).
        pub prompt_error: Option<String>,
        /// Number of initial `prompt` calls to fail (for testing retry-after-error).
        pub fail_prompt_count: Arc<std::sync::atomic::AtomicUsize>,
        /// Records every `prompt` call's text (asserts retry re-submits).
        pub prompt_calls: Arc<tokio::sync::Mutex<Vec<String>>>,
        /// Records every `prompt_async` call's text (asserts supplement path).
        pub prompt_async_calls: Arc<tokio::sync::Mutex<Vec<String>>>,
        /// The session id `create_session` returns.
        pub session_id: String,
        /// When true, `prompt` 404s for any session id other than `session_id`
        /// (simulates a stale mapping to a session that no longer exists).
        pub stale_session_404: bool,
        /// child session_id → parent session_id, served by `session_info`
        /// (simulates sub-task sessions created by the `task` tool).
        pub session_parents: std::collections::HashMap<String, String>,
        /// The shared store served by `list_sessions` (for `/list`, `/attach`,
        /// `/switch` tests).
        pub session_list: Vec<opencode::client::SessionListInfo>,
        /// Records `update_session_title` calls: (session_id, title).
        pub update_title_calls: Arc<tokio::sync::Mutex<Vec<(String, String)>>>,
        /// Counts `list_sessions` invocations (asserts the 30 s cache).
        pub list_sessions_calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl MockBackend {
        pub fn new(parts: serde_json::Value) -> Self {
            Self {
                parts,
                permissions: Vec::new(),
                external_user_message: None,
                reply_permission_calls: Arc::new(tokio::sync::Mutex::new(Vec::new())),
                session_titles: std::sync::Mutex::new(std::collections::HashMap::new()),
                questions: Vec::new(),
                reply_question_calls: Arc::new(tokio::sync::Mutex::new(Vec::new())),
                prompt_error: None,
                fail_prompt_count: std::sync::atomic::AtomicUsize::new(0).into(),
                prompt_calls: Arc::new(tokio::sync::Mutex::new(Vec::new())),
                prompt_async_calls: Arc::new(tokio::sync::Mutex::new(Vec::new())),
                session_id: "ses_test".into(),
                stale_session_404: false,
                session_parents: std::collections::HashMap::new(),
                session_list: Vec::new(),
                update_title_calls: Arc::new(tokio::sync::Mutex::new(Vec::new())),
                list_sessions_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            }
        }
    }

    #[async_trait::async_trait]
    impl opencode::Backend for MockBackend {
        fn new_session_input(&self, directory: Option<&str>) -> opencode::client::CreateSessionInput {
            opencode::client::CreateSessionInput {
                id: None,
                agent: None,
                model: Some(opencode::client::ModelInfo {
                    id: "m".into(),
                    provider_id: "p".into(),
                    variant: None,
                }),
                location: directory.map(|d| opencode::client::Location {
                    directory: d.to_string(),
                }),
            }
        }

        async fn create_session(
            &self,
            _i: &opencode::client::CreateSessionInput,
        ) -> crate::error::Result<opencode::client::Session> {
            Ok(opencode::client::Session {
                id: self.session_id.clone(),
                project_id: None,
                agent: None,
                title: None,
                location: None,
                cost: None,
                time: None,
            })
        }

        async fn list_sessions(
            &self,
        ) -> crate::error::Result<Vec<opencode::client::SessionListInfo>> {
            self.list_sessions_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(self.session_list.clone())
        }

        async fn update_session_title(&self, session_id: &str, title: &str) -> crate::error::Result<()> {
            self.update_title_calls
                .lock()
                .await
                .push((session_id.to_string(), title.to_string()));
            self.session_titles
                .lock()
                .unwrap()
                .insert(session_id.to_string(), title.to_string());
            Ok(())
        }

        async fn prompt(
            &self,
            session_id: &str,
            text: &str,
        ) -> crate::error::Result<opencode::client::PromptResponse> {
            self.prompt_calls.lock().await.push(text.to_string());
            if self.stale_session_404 && session_id != self.session_id {
                return Err(crate::error::BridgeError::SessionNotFound(session_id.to_string()));
            }
            if self.fail_prompt_count.load(std::sync::atomic::Ordering::SeqCst) > 0 {
                self.fail_prompt_count
                    .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                return Err(crate::error::BridgeError::OpenCode(
                    "Simulated provider failure".into(),
                ));
            }
            if let Some(err) = &self.prompt_error {
                return Err(crate::error::BridgeError::OpenCode(err.clone()));
            }
            Ok(opencode::client::PromptResponse {
                id: "msg_assist".into(),
                session_id: Some(session_id.to_string()),
                admitted_seq: None,
                parent_id: Some("msg_user".into()),
                error: None,
                parts: self.parts.clone(),
            })
        }

        async fn prompt_async(&self, session_id: &str, text: &str) -> crate::error::Result<()> {
            self.prompt_async_calls
                .lock()
                .await
                .push(format!("{}:{}", session_id, text));
            Ok(())
        }

        async fn messages(
            &self,
            _session_id: &str,
        ) -> crate::error::Result<Vec<opencode::client::SessionMessage>> {
            let now = chrono::Utc::now().timestamp_millis();
            // When set, simulate a user message posted by ANOTHER client (e.g.
            // OpenChamber), for the external-message poller tests.
            if let Some(text) = &self.external_user_message {
                return Ok(vec![opencode::client::SessionMessage {
                    info: opencode::client::MessageInfo {
                        id: "msg_ext_user".into(),
                        role: Some("user".into()),
                        parent_id: None,
                        time: Some(opencode::client::MessageTime { created: now }),
                        model_id: None,
                        provider_id: None,
                        tokens: None,
                    },
                    parts: serde_json::json!([{ "type": "text", "text": text }]),
                }]);
            }
            Ok(vec![opencode::client::SessionMessage {
                info: opencode::client::MessageInfo {
                    id: "msg_assist".into(),
                    role: Some("assistant".into()),
                    parent_id: Some("msg_user".into()),
                    time: Some(opencode::client::MessageTime { created: now + 1000 }),
                    model_id: None,
                    provider_id: None,
                    tokens: None,
                },
                parts: self.parts.clone(),
            }])
        }

        async fn list_permissions(
            &self,
            _d: Option<&str>,
        ) -> crate::error::Result<Vec<opencode::client::PermissionRequest>> {
            Ok(self.permissions.clone())
        }

        async fn list_questions(
            &self,
            _d: Option<&str>,
        ) -> crate::error::Result<Vec<opencode::client::QuestionRequest>> {
            Ok(self.questions.clone())
        }

        async fn model_context_window(
            &self,
            _provider: &str,
            _model: &str,
        ) -> crate::error::Result<Option<i64>> {
            Ok(Some(100_000))
        }

        async fn reply_question(
            &self,
            request_id: &str,
            answers: &[Vec<String>],
            _d: Option<&str>,
        ) -> crate::error::Result<()> {
            self.reply_question_calls
                .lock()
                .await
                .push((request_id.to_string(), answers.to_vec()));
            Ok(())
        }

        async fn reject_question(&self, request_id: &str, _d: Option<&str>) -> crate::error::Result<()> {
            self.reply_question_calls
                .lock()
                .await
                .push((request_id.to_string(), vec![vec!["__reject__".to_string()]]));
            Ok(())
        }

        async fn reply_permission(&self, r: &str, reply: &str, _d: Option<&str>) -> crate::error::Result<()> {
            self.reply_permission_calls
                .lock()
                .await
                .push((r.to_string(), reply.to_string()));
            Ok(())
        }

        async fn session_info(
            &self,
            session_id: &str,
            _d: Option<&str>,
        ) -> crate::error::Result<opencode::client::SessionInfo> {
            Ok(opencode::client::SessionInfo {
                id: session_id.to_string(),
                parent_id: self.session_parents.get(session_id).cloned(),
                title: self.session_titles.lock().unwrap().get(session_id).cloned(),
            })
        }

        async fn interrupt(&self, _s: &str) -> crate::error::Result<()> {
            Ok(())
        }
        async fn compact(&self, _s: &str) -> crate::error::Result<()> {
            Ok(())
        }
        async fn switch_agent(&self, _s: &str, _a: &str) -> crate::error::Result<()> {
            Ok(())
        }
        async fn switch_model(&self, _s: &str, _m: &str) -> crate::error::Result<()> {
            Ok(())
        }
        async fn reconnect(&self, _url: &str, _password: &str) -> crate::error::Result<()> {
            Ok(())
        }
        fn base_url(&self) -> String {
            "http://mock".into()
        }
    }

    pub fn test_config(session_file: &std::path::Path) -> crate::config::Config {
        crate::config::Config {
            opencode: crate::config::OpenCodeConfig {
                url: "http://localhost:1".into(),
                username: None,
                password: None,
                model: "test/model".into(),
            },
            feishu: crate::config::FeishuConfig {
                app_id: "app".into(),
                app_secret: "secret".into(),
            },
            bridge: crate::config::BridgeConfig {
                session_file: session_file.to_path_buf(),
                work_dir: None,
                group_completion_notice: true,
            },
        }
    }

    /// The parts a real assistant turn produces: reasoning → tool → text.
    pub fn realistic_parts() -> serde_json::Value {
        serde_json::json!([
            { "id": "prt_s1", "type": "step-start", "snapshot": "x" },
            { "id": "prt_r1", "type": "reasoning", "text": "用户想让我分析目录。" },
            { "id": "prt_t1", "type": "tool", "tool": "bash", "callID": "call_1",
              "state": { "status": "completed", "input": { "command": "ls -la" }, "output": "src/\nCargo.toml\n" } },
            { "id": "prt_f1", "type": "step-finish", "reason": "tool-calls" },
            { "id": "prt_s2", "type": "step-start", "snapshot": "x" },
            { "id": "prt_txt", "type": "text", "text": "当前目录有 src/ 和 Cargo.toml。" },
            { "id": "prt_f2", "type": "step-finish", "reason": "stop" },
        ])
    }

    /// A prompt whose answer is far longer than one card's text budget, so it
    /// must flow across continuation cards (no plain-text fallback anymore).
    pub fn long_answer_parts() -> serde_json::Value {
        // 1200 × 6 chars = 7200 chars, above MAX_CARD_TEXT_CHARS (6000).
        let long_text = "很长的回答。".repeat(1200);
        serde_json::json!([
            { "id": "prt_s1", "type": "step-start", "snapshot": "x" },
            { "id": "prt_txt", "type": "text", "text": long_text },
            { "id": "prt_f1", "type": "step-finish", "reason": "stop" },
        ])
    }
}

#[cfg(test)]
mod integration_tests {
    use super::test_support::*;
    use super::*;
    use crate::bridge::command::Command;

    async fn build_app(
        cfg: crate::config::Config,
        backend: MockBackend,
    ) -> (Arc<App>, Arc<RecordingPlatform>) {
        let platform = Arc::new(RecordingPlatform::new());
        let app = Arc::new(App::new(cfg, Arc::new(backend), platform.clone()).unwrap());
        (app, platform)
    }

    /// Create a temp work dir, set it as the process cwd (sessions are created
    /// in cwd, and tests must never operate in the cola repo) and return it.
    /// The returned TempDir must stay alive for the test's duration.
    fn test_work_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        dir
    }

    #[tokio::test]
    async fn handle_prompt_renders_reasoning_tools_and_text() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

        app.handle_message(
            "msg_1".into(),
            "chat_1".into(),
            "p2p".into(),
            None,
            "分析一下目录".into(),
            None,
        )
        .await;

        let calls = platform.calls.lock().await.clone();
        // First call must be the Loading reply card.
        assert!(matches!(calls.first(), Some(PlatformCall::ReplyCard { .. })));
        // At least one card update (flush) must follow.
        let updates: Vec<_> = calls
            .iter()
            .filter_map(|c| match c {
                PlatformCall::UpdateMessage { card, .. } => Some(card.clone()),
                _ => None,
            })
            .collect();
        assert!(!updates.is_empty(), "expected card updates, got: {:?}", calls);

        let final_card = updates.last().unwrap().clone();
        let text = final_card.to_string();
        assert!(text.contains("✅"), "final header should be Done: {}", text);
        assert!(text.contains("推理过程"), "reasoning panel missing: {}", text);
        assert!(text.contains("bash"), "tool panel missing: {}", text);
        assert!(
            text.contains("当前目录有 src/ 和 Cargo.toml。"),
            "text missing: {}",
            text
        );
        assert!(text.contains("ls -la"), "tool input missing: {}", text);
    }

    #[tokio::test]
    async fn prompt_error_renders_error_card() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut backend = MockBackend::new(realistic_parts());
        backend.prompt_error = Some("Streaming response failed: [503] The request queue is full.".into());
        let (app, platform) = build_app(cfg, backend).await;

        app.handle_message(
            "msg_1".into(),
            "chat_1".into(),
            "p2p".into(),
            None,
            "hi".into(),
            None,
        )
        .await;

        let calls = platform.calls.lock().await.clone();
        let updates: Vec<_> = calls
            .iter()
            .filter_map(|c| match c {
                PlatformCall::UpdateMessage { card, .. } => Some(card.clone()),
                _ => None,
            })
            .collect();
        assert!(
            !updates.is_empty(),
            "expected an error card update, got: {:?}",
            calls
        );
        let card = updates.last().unwrap().to_string();
        assert!(card.contains("❌"), "error card header missing: {}", card);
        assert!(card.contains("503"), "error text missing: {}", card);
    }

    #[tokio::test]
    async fn error_card_retry_reuses_card_and_reruns_prompt() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        // First prompt fails (provider hiccup); the retry must succeed.
        let mock = MockBackend::new(realistic_parts());
        mock.fail_prompt_count
            .store(1, std::sync::atomic::Ordering::SeqCst);
        let prompt_calls = mock.prompt_calls.clone();
        let backend = Arc::new(mock);
        let platform = Arc::new(RecordingPlatform::new());
        let app = Arc::new(App::new(cfg, backend, platform.clone()).unwrap());

        app.handle_message(
            "msg_1".into(),
            "chat_1".into(),
            "p2p".into(),
            None,
            "hi".into(),
            None,
        )
        .await;

        // The error card must carry a retry button.
        let calls = platform.calls.lock().await.clone();
        let updates: Vec<_> = calls
            .iter()
            .filter_map(|c| match c {
                PlatformCall::UpdateMessage { card, .. } => Some(card.clone()),
                _ => None,
            })
            .collect();
        let error_card = updates.last().unwrap().clone();
        let err_text = error_card.to_string();
        assert!(err_text.contains("❌"), "error card missing: {}", err_text);
        assert!(err_text.contains("重试"), "retry button missing: {}", err_text);

        // The card the retry will reuse: the loading reply card id.
        let card_id = match calls.first().unwrap() {
            PlatformCall::ReplyCard { card, .. } if card.to_string().contains("思考中") => "msg_reply",
            _ => panic!("expected a loading reply card first: {:?}", calls),
        };
        assert_eq!(card_id, "msg_reply");

        // User clicks the retry button.
        let retry = app
            .handle_card_action(serde_json::json!({ "action": "retry", "session_id": "ses_test" }))
            .await;
        assert!(retry.is_some(), "retry ack card expected");
        assert_eq!(retry.unwrap().toast.as_deref(), Some("正在重试..."));

        // The spawned retry re-runs the prompt on the SAME card, not a new reply.
        tokio::time::sleep(std::time::Duration::from_millis(2500)).await;

        let calls = platform.calls.lock().await.clone();
        let updates: Vec<_> = calls
            .iter()
            .filter_map(|c| match c {
                PlatformCall::UpdateMessage { message_id, card } => Some((message_id.clone(), card.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(
            updates.last().unwrap().0,
            "msg_reply",
            "retry must update the original card, not send a new one: {:?}",
            calls
        );
        let final_card = updates.last().unwrap().1.to_string();
        assert!(
            final_card.contains("✅"),
            "retry should finish Done: {}",
            final_card
        );
        assert!(
            final_card.contains("当前目录有 src/ 和 Cargo.toml。"),
            "retried answer missing: {}",
            final_card
        );

        let backend_calls = prompt_calls.lock().await.clone();
        assert_eq!(backend_calls, vec!["hi".to_string(), "hi".to_string()]);
    }

    #[tokio::test]
    async fn group_completion_sends_notice_to_requester() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

        app.handle_message(
            "msg_1".into(),
            "oc_group_1".into(),
            "group".into(),
            None,
            "hi".into(),
            Some("ou_requester".into()),
        )
        .await;

        let calls = platform.calls.lock().await.clone();
        let notices: Vec<_> = calls
            .iter()
            .filter_map(|c| match c {
                PlatformCall::CompletionNotice {
                    reply_to,
                    open_id,
                    text,
                    ..
                } => Some((reply_to.clone(), open_id.clone(), text.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(
            notices,
            vec![(
                "msg_1".to_string(),
                "ou_requester".to_string(),
                "✅ 已完成。".to_string()
            )]
        );
    }

    #[tokio::test]
    async fn group_completion_at_mentions_requester_when_name_resolvable() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut rp = RecordingPlatform::new();
        rp.user_names
            .insert("ou_requester".to_string(), "李明".to_string());
        let platform = Arc::new(rp);
        let app = Arc::new(
            App::new(
                cfg,
                Arc::new(MockBackend::new(realistic_parts())),
                platform.clone(),
            )
            .unwrap(),
        );

        app.handle_message(
            "msg_1".into(),
            "oc_group_1".into(),
            "group".into(),
            None,
            "hi".into(),
            Some("ou_requester".into()),
        )
        .await;

        let calls = platform.calls.lock().await.clone();
        let notices: Vec<_> = calls
            .iter()
            .filter_map(|c| match c {
                PlatformCall::CompletionNotice {
                    reply_to,
                    open_id,
                    name,
                    text,
                } => Some((reply_to.clone(), open_id.clone(), name.clone(), text.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(
            notices,
            vec![(
                "msg_1".to_string(),
                "ou_requester".to_string(),
                Some("李明".to_string()),
                "✅ 已完成。".to_string()
            )]
        );
    }

    #[tokio::test]
    async fn p2p_prompt_sends_no_completion_notice() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

        app.handle_message(
            "msg_1".into(),
            "oc_p2p_1".into(),
            "p2p".into(),
            None,
            "hi".into(),
            Some("ou_user".into()),
        )
        .await;

        let calls = platform.calls.lock().await.clone();
        assert!(
            !calls
                .iter()
                .any(|c| matches!(c, PlatformCall::CompletionNotice { .. })),
            "p2p must not send a completion notice: {:?}",
            calls
        );
    }

    #[tokio::test]
    async fn subtitle_falls_back_to_id_tail_without_server_title() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let (app, _) = build_app(cfg, MockBackend::new(realistic_parts())).await;
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());

        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: key.clone(),
                session_id: "ses_01ba0ed03ffeRvYNWua6mg8d9c".into(),
                directory: "/tmp/x".into(),
                agent: None,
                auto_accept: false,
                topic_anchor: None,
            });
        }

        // No server title → the id-tail alone identifies the session (no cola
        // side name to fall back on; the current prompt is never echoed).
        assert_eq!(app.session_subtitle(&key, "另一个问题").await, "01ba0ed");
        assert_eq!(app.session_subtitle(&key, "你好").await, "01ba0ed");
    }

    /// A server default title (`New session - ...`) is treated as absent — the
    /// id-tail is shown until the server generates a real title.
    #[tokio::test]
    async fn subtitle_ignores_server_default_title() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mock = MockBackend::new(realistic_parts());
        mock.session_titles.lock().unwrap().insert("ses_00ea4e77cffez1fo4wrNuJyHF0".into(), "New session - 2026-08-28".into());
        let (app, _) = build_app(cfg, mock).await;
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());

        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: key.clone(),
                session_id: "ses_00ea4e77cffez1fo4wrNuJyHF0".into(),
                directory: "/tmp/y".into(),
                agent: None,
                auto_accept: false,
                topic_anchor: None,
            });
        }
        assert_eq!(app.session_subtitle(&key, "另一个问题").await, "00ea4e7");
    }

    /// The card subtitle prefers the OpenCode server's own session title (what
    /// OpenChamber shows), not cola's `/new`-generated names.
    #[tokio::test]
    async fn subtitle_prefers_server_title() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mock = MockBackend::new(realistic_parts());
        mock.session_titles.lock().unwrap().insert("ses_test".into(), "OpenChamber 显示的标题".into());
        let (app, _) = build_app(cfg, mock).await;
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());

        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: key.clone(),
                session_id: "ses_test".into(),
                directory: "/tmp/x".into(),
                agent: None,
                auto_accept: false,
                topic_anchor: None,
            });
        }
        assert_eq!(
            app.session_subtitle(&key, "问题").await,
            "OpenChamber 显示的标题 · test"
        );
    }

    /// The card subtitle follows the server's live session title during a turn:
    /// OpenCode auto-renames a session after streaming, and `refresh_session_title`
    /// (called from the render poll loop) must update the card instead of leaving
    /// it on the "new session" default until restart.
    #[tokio::test]
    async fn refresh_session_title_updates_live_card_on_server_rename() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mock = MockBackend::new(realistic_parts());
        // The server already has a final auto-generated title.
        mock.session_titles.lock().unwrap().insert("ses_test".into(), "修复登录鉴权问题".into());
        let backend = Arc::new(mock);
        let platform = Arc::new(RecordingPlatform::new());
        let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());

        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: key.clone(),
                session_id: "ses_test".into(),
                directory: "/tmp/x".into(),
                agent: None,
                auto_accept: false,
                topic_anchor: None,
            });
        }

        // Simulate an in-flight turn whose card was captured with the OLD
        // default subtitle before the server auto-titled the session.
        let mut acc = crate::bridge::streaming::StreamAccumulator::new("test");
        acc.reply_to_message_id = Some("msg_1".into());
        acc.session_id = Some("ses_test".into());
        {
            let mut accs = app.accumulators.lock().await;
            accs.insert("ses_test".into(), acc);
        }

        // The server's live title differs → refresh must update the card subtitle.
        let refreshed = app.refresh_session_title("ses_test").await;
        assert!(refreshed, "a server rename must refresh the card title");
        let title = app
            .accumulators
            .lock()
            .await
            .get("ses_test")
            .unwrap()
            .title
            .clone();
        assert_eq!(title, "修复登录鉴权问题 · test");
        // A second refresh with no further change must be a no-op (no churn).
        assert!(
            !app.refresh_session_title("ses_test").await,
            "no change when the title already matches"
        );
        // No accumulator (a finished turn) → refresh is a no-op.
        app.accumulators.lock().await.remove("ses_test");
        assert!(!app.refresh_session_title("ses_test").await);
    }

    #[tokio::test]
    async fn long_answer_splits_across_cards_no_plain_text() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let (app, platform) = build_app(cfg, MockBackend::new(long_answer_parts())).await;

        app.handle_message(
            "msg_1".into(),
            "oc_p2p_1".into(),
            "p2p".into(),
            None,
            "hi".into(),
            None,
        )
        .await;

        let calls = platform.calls.lock().await.clone();
        // Every message cola sent must be a CARD (ReplyCard / UpdateMessage) —
        // no separate plain-text message for long answers.
        assert!(
            calls.iter().all(|c| {
                matches!(
                    c,
                    PlatformCall::ReplyCard { .. } | PlatformCall::UpdateMessage { .. }
                )
            }),
            "long answer must stay on cards, got: {:?}",
            calls
        );
        // The FULL text must be present across the cards (not truncated).
        let all_cards: String = calls
            .iter()
            .filter_map(|c| match c {
                PlatformCall::ReplyCard { card, .. } => Some(card.to_string()),
                PlatformCall::UpdateMessage { card, .. } => Some(card.to_string()),
                _ => None,
            })
            .collect();
        assert!(
            all_cards.contains("很长的回答。"),
            "full answer must appear on a card: {}",
            all_cards
        );
        let expected = "很长的回答。".repeat(1200);
        assert!(
            all_cards.chars().filter(|c| *c != '"').count() >= expected.chars().count(),
            "all text must be delivered (preview-only would lose the tail)"
        );
    }

    #[tokio::test]
    async fn short_answer_stays_in_card_no_extra_message() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

        app.handle_message(
            "msg_1".into(),
            "oc_p2p_1".into(),
            "p2p".into(),
            None,
            "hi".into(),
            None,
        )
        .await;

        let calls = platform.calls.lock().await.clone();
        assert!(
            calls.iter().all(|c| matches!(
                c,
                PlatformCall::ReplyCard { .. } | PlatformCall::UpdateMessage { .. }
            )),
            "answer must stay on cards: {:?}",
            calls
        );
    }

    #[tokio::test]
    async fn permission_poller_sends_card_and_card_action_replies() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut backend = MockBackend::new(realistic_parts());
        backend.permissions = vec![opencode::client::PermissionRequest {
            request_id: "per_1".into(),
            session_id: Some("ses_test".into()),
            permission: Some("bash".into()),
            patterns: vec!["ls -la".into()],
            metadata: None,
            always: Vec::new(),
        }];
        let (app, _platform) = build_app(cfg, backend).await;

        // Seed a session + accumulator so the poller has a reply target.
        app.handle_message(
            "msg_1".into(),
            "chat_1".into(),
            "p2p".into(),
            None,
            "hi".into(),
            None,
        )
        .await;

        // Run the permission poller briefly.
        tokio::spawn({
            let app = app.clone();
            async move {
                let _ = app.permission.poll_loop(&app).await;
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(3500)).await;

        // The session has an active streaming card, so the permission is surfaced
        // INLINE on it (one-card-per-turn) — not as a separate card.
        let perm_inline = app
            .accumulators
            .lock()
            .await
            .get("ses_test")
            .expect("accumulator exists")
            .pending_permissions
            .clone();
        assert_eq!(perm_inline.len(), 1, "permission should be inlined");
        assert_eq!(perm_inline[0].request_id, "per_1");
        // The streaming card itself renders the inline permission section.
        let card = app
            .accumulators
            .lock()
            .await
            .get("ses_test")
            .unwrap()
            .build_card();
        let card_text = card.to_string();
        assert!(
            card_text.contains("权限请求"),
            "inline section missing: {}",
            card_text
        );
        assert!(
            card_text.contains("ls -la"),
            "permission body missing: {}",
            card_text
        );
        assert!(
            card_text.contains("Allow Once"),
            "allow button missing: {}",
            card_text
        );

        // Simulate the user clicking "Allow Once" — answered inline, so the ack
        // carries a toast but must NOT replace the streaming card.
        let value = serde_json::json!({
            "action": "perm",
            "reply": "once",
            "session_id": "ses_test",
            "request_id": "per_1",
            "perm_label": "✅ Allowed once",
            "perm_color": "green",
            "perm_body": "bash",
        });
        let result = app.handle_card_action(value).await;
        assert!(result.is_some());
        let result = result.unwrap();
        assert!(
            result.card.is_none(),
            "inline answer must not replace the streaming card"
        );
        // A toast gives the client instant feedback on the button press.
        assert_eq!(result.toast.as_deref(), Some("已允许本次执行"));
        // The inline section is removed from the accumulator.
        assert!(
            app.accumulators
                .lock()
                .await
                .get("ses_test")
                .unwrap()
                .pending_permissions
                .is_empty()
        );
    }

    #[tokio::test]
    async fn auto_accept_session_answers_permission_without_card() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut mock = MockBackend::new(realistic_parts());
        mock.permissions = vec![opencode::client::PermissionRequest {
            request_id: "per_aa".into(),
            session_id: Some("ses_test".into()),
            permission: Some("bash".into()),
            patterns: vec!["ls".into()],
            metadata: None,
            always: Vec::new(),
        }];
        let perm_calls = mock.reply_permission_calls.clone();
        let (app, platform) = build_app(cfg, mock).await;

        // Enable `/autoaccept` on the session.
        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
                session_id: "ses_test".into(),
                directory: "/tmp/aa".into(),
                agent: None,
                auto_accept: true,
                topic_anchor: None,
            });
            store.persist().unwrap();
        }

        tokio::spawn({
            let app = app.clone();
            async move {
                let _ = app.permission.poll_loop(&app).await;
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(3500)).await;

        // Auto-accepted: reply_permission called with "once", no card sent.
        let calls = perm_calls.lock().await.clone();
        assert_eq!(
            calls,
            vec![("per_aa".to_string(), "once".to_string())],
            "auto-accept should reply once"
        );
        let sent = platform.calls.lock().await.clone();
        assert!(
            !sent.iter().any(|c| {
                if let PlatformCall::ReplyCard { card, .. } = c {
                    card.to_string().contains("Permission Required")
                } else {
                    false
                }
            }),
            "no permission card should be sent for an auto-accept session: {:?}",
            sent
        );
    }

    /// `/autoaccept on` must also approve requests that were ALREADY pending
    /// (seen before the flag existed), not just future ones. Otherwise the
    /// poller's `seen` set leaves old permission cards hanging forever.
    #[tokio::test]
    async fn autoaccept_on_approves_already_pending_permission() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut mock = MockBackend::new(realistic_parts());
        mock.permissions = vec![opencode::client::PermissionRequest {
            request_id: "per_pending".into(),
            session_id: Some("ses_test".into()),
            permission: Some("bash".into()),
            patterns: vec!["ls -la".into()],
            metadata: None,
            always: Vec::new(),
        }];
        let perm_calls = mock.reply_permission_calls.clone();
        let (app, _platform) = build_app(cfg, mock).await;

        // Session already mapped but autoaccept OFF — the permission would have
        // been surfaced as a card before the user enabled it.
        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
                session_id: "ses_test".into(),
                directory: "/tmp/aa".into(),
                agent: None,
                auto_accept: false,
                topic_anchor: None,
            });
            store.persist().unwrap();
        }

        // Now the user turns autoaccept on via the command.
        app.handle_command(
            Command::AutoAccept(crate::bridge::command::AutoAcceptAction::Set(true)),
            crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            "msg_cmd",
            crate::config::ConversationKind::P2p,
        )
        .await
        .unwrap();

        // The already-pending request was approved with "once" immediately.
        let calls = perm_calls.lock().await.clone();
        assert_eq!(
            calls,
            vec![("per_pending".to_string(), "once".to_string())],
            "turning autoaccept on should approve already-pending requests"
        );
        // The flag is persisted for future requests.
        let entry = {
            let store = app.sessions.lock().await;
            store
                .get_active(&crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()))
                .cloned()
        };
        assert!(entry.unwrap().auto_accept, "autoaccept flag should persist");
    }

    /// Sub-task child sessions carry their own sessionID; `/autoaccept on` on
    /// the parent must reach through the parent chain and approve their
    /// pending permissions too.
    #[tokio::test]
    async fn autoaccept_on_approves_child_session_permission() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut mock = MockBackend::new(realistic_parts());
        mock.permissions = vec![opencode::client::PermissionRequest {
            request_id: "per_child".into(),
            session_id: Some("ses_child".into()),
            permission: Some("bash".into()),
            patterns: vec!["rm -rf x".into()],
            metadata: None,
            always: Vec::new(),
        }];
        mock.session_parents.insert("ses_child".into(), "ses_test".into());
        let perm_calls = mock.reply_permission_calls.clone();
        let (app, _platform) = build_app(cfg, mock).await;

        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
                session_id: "ses_test".into(),
                directory: "/tmp/aa".into(),
                agent: None,
                auto_accept: false,
                topic_anchor: None,
            });
            store.persist().unwrap();
        }

        app.handle_command(
            Command::AutoAccept(crate::bridge::command::AutoAcceptAction::Set(true)),
            crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            "msg_cmd",
            crate::config::ConversationKind::P2p,
        )
        .await
        .unwrap();

        let calls = perm_calls.lock().await.clone();
        assert_eq!(
            calls,
            vec![("per_child".to_string(), "once".to_string())],
            "child-session permission should be approved via the parent chain"
        );
    }

    #[tokio::test]
    async fn stale_permission_card_marked_handled_when_resolved_elsewhere() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        // No pending permissions on the server — the card cola sent is stale.
        let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
        app.permission
            .sent_permission_cards
            .lock()
            .await
            .insert("per_stale".into(), ("om_sent_card".into(), "bash ls -la".into()));

        tokio::spawn({
            let app = app.clone();
            async move {
                let _ = app.permission.poll_loop(&app).await;
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(3500)).await;

        let calls = platform.calls.lock().await.clone();
        let stale = calls.iter().find_map(|c| match c {
            PlatformCall::UpdateMessage { message_id, card } if message_id == "om_sent_card" => {
                Some(card.clone())
            }
            _ => None,
        });
        let stale = stale.expect("stale permission card should be marked");
        assert!(
            stale.to_string().contains("已处理"),
            "stale card should show as handled: {}",
            stale
        );
        assert!(
            stale.to_string().contains("bash ls -la"),
            "stale card should keep the original request text: {}",
            stale
        );
    }

    #[tokio::test]
    async fn external_message_from_shared_store_notifies_feishu() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut mock = MockBackend::new(realistic_parts());
        mock.external_user_message = Some("OpenChamber 里发的消息".to_string());
        let (app, platform) = build_app(cfg, mock).await;

        // A known session whose chat the notification goes to.
        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: crate::config::ThreadKey::new("oc_group_1".into(), "oc_group_1".into()),
                session_id: "ses_ext".into(),
                directory: "/tmp/ext".into(),
                agent: None,
                auto_accept: false,
                topic_anchor: None,
            });
            store.persist().unwrap();
        }
        // Baseline: a minute ago, so the fresh user message is "new".
        let baseline = chrono::Utc::now().timestamp_millis() - 60_000;
        app.external
            .last_user_msg_epoch
            .lock()
            .await
            .insert("ses_ext".into(), baseline);

        tokio::spawn({
            let app = app.clone();
            async move {
                let _ = app.external.poll_loop(&app).await;
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(8500)).await;

        let calls = platform.calls.lock().await.clone();
        let notify = calls.iter().find_map(|c| match c {
            PlatformCall::SendCard { card, .. } if card.to_string().contains("有新消息") => {
                Some(card.clone())
            }
            _ => None,
        });
        let notify = notify.expect("external message should produce a notification card");
        assert!(
            notify.to_string().contains("OpenChamber 里发的消息"),
            "notification should preview the message: {}",
            notify
        );
    }

    /// External messages to a TOPIC-backed session are notified by replying to
    /// a message INSIDE the topic, not sent to the chat top level. Covers the
    /// no-persisted-anchor case (session created before `/topic` stored the
    /// anchor): `resolve_topic_anchor` queries the thread for the newest bot
    /// message and replies to it.
    #[tokio::test]
    async fn external_message_to_topic_session_notifies_into_thread() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut mock = MockBackend::new(realistic_parts());
        mock.external_user_message = Some("话题里的外部消息".to_string());
        let (app, platform) = build_app(cfg, mock).await;

        // A TOPIC-backed session (thread_id != chat_id) with NO persisted
        // anchor (like the old /topic sessions) — the anchor must be resolved
        // by querying the thread.
        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: crate::config::ThreadKey::new("chat_1".into(), "omt_topic_ext".into()),
                session_id: "ses_ext".into(),
                directory: "/tmp/ext".into(),
                agent: None,
                auto_accept: false,
                topic_anchor: None,
            });
            store.persist().unwrap();
        }
        let baseline = chrono::Utc::now().timestamp_millis() - 60_000;
        app.external
            .last_user_msg_epoch
            .lock()
            .await
            .insert("ses_ext".into(), baseline);

        tokio::spawn({
            let app = app.clone();
            async move {
                let _ = app.external.poll_loop(&app).await;
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(8500)).await;

        let calls = platform.calls.lock().await.clone();
        assert!(
            calls.iter().any(|c| matches!(
                c,
                PlatformCall::ReplyCard { reply_to, card } if reply_to == "msg_in_topic_anchor" && card.to_string().contains("有新消息")
            )),
            "topic external notification should reply into the topic (resolved anchor): {calls:?}"
        );
        assert!(
            !calls
                .iter()
                .any(|c| matches!(c, PlatformCall::SendCard { receive_id, .. } if receive_id == "chat_1")),
            "topic external notification must NOT go to chat top level: {calls:?}"
        );
    }

    #[tokio::test]
    async fn subtask_permission_routes_to_mapped_parent_and_reply_carries_directory() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut backend = MockBackend::new(realistic_parts());
        // A sub-task session cola never created; its permission must be routed
        // up to the parent session's chat.
        let child = "ses_child_task";
        backend
            .session_parents
            .insert(child.into(), backend.session_id.clone());
        backend.permissions = vec![opencode::client::PermissionRequest {
            request_id: "per_child".into(),
            session_id: Some(child.into()),
            permission: Some("bash".into()),
            patterns: vec!["git status".into()],
            metadata: None,
            always: Vec::new(),
        }];
        let parent_id = backend.session_id.clone();
        let (app, _platform) = build_app(cfg, backend).await;

        // Seed the parent session so it maps child → parent → chat.
        app.handle_message(
            "msg_1".into(),
            "chat_1".into(),
            "p2p".into(),
            None,
            "hi".into(),
            None,
        )
        .await;

        tokio::spawn({
            let app = app.clone();
            async move {
                let _ = app.permission.poll_loop(&app).await;
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(3500)).await;

        // The parent session has a live streaming card, so the child's
        // permission is INLINED on it (one-card-per-turn), not sent as a
        // separate card — the child itself has no accumulator, so it must be
        // hosted on the parent's card found by walking the parent chain.
        let perm_inline = app
            .accumulators
            .lock()
            .await
            .get(&parent_id)
            .expect("parent accumulator exists")
            .pending_permissions
            .clone();
        assert_eq!(
            perm_inline.len(),
            1,
            "subtask permission should be inlined on the parent card"
        );
        assert_eq!(perm_inline[0].request_id, "per_child");
        assert_eq!(perm_inline[0].session_id, child);

        // The streaming card renders the inline section with the child's buttons.
        let card = app
            .accumulators
            .lock()
            .await
            .get(&parent_id)
            .unwrap()
            .build_card();
        let card_text = card.to_string();
        assert!(card_text.contains("权限请求"), "inline section missing");
        assert!(card_text.contains("git status"), "permission body missing");
        // The button carries the CHILD session id (the request's owner) plus the
        // owning directory so the reply routes to the right instance even though
        // the child session isn't in the store.
        let first_button = card["body"]["elements"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["tag"] == "button")
            .expect("permission buttons present");
        let value = first_button["value"].clone();
        assert_eq!(value["session_id"], child);
        assert!(
            value["directory"]
                .as_str()
                .map(|d| !d.is_empty())
                .unwrap_or(false),
            "permission card must carry a directory, got: {}",
            value
        );

        // Clicking Allow routes the reply with that directory and drops the
        // inline section (no replacement card — the streaming card re-renders).
        let mut value = value;
        value["reply"] = serde_json::json!("once");
        value["perm_label"] = serde_json::json!("✅ Allowed once");
        value["perm_color"] = serde_json::json!("green");
        let result = app.handle_card_action(value).await;
        assert!(result.is_some(), "reply should succeed for subtask session");
        assert!(
            result.unwrap().card.is_none(),
            "inline answer must not replace the streaming card"
        );
        assert!(
            app.accumulators
                .lock()
                .await
                .get(&parent_id)
                .unwrap()
                .pending_permissions
                .is_empty(),
            "inline permission section should be removed after answering"
        );
    }

    /// Without a live streaming card (e.g. the parent turn finished or cola
    /// restarted), a sub-task child's permission falls back to a separate card
    /// sent into the parent's chat — still routed up the parent chain, never
    /// dropped.
    #[tokio::test]
    async fn subtask_permission_without_streaming_card_sends_card_to_parent_chat() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut backend = MockBackend::new(realistic_parts());
        let child = "ses_child_task";
        backend
            .session_parents
            .insert(child.into(), backend.session_id.clone());
        backend.permissions = vec![opencode::client::PermissionRequest {
            request_id: "per_child".into(),
            session_id: Some(child.into()),
            permission: Some("bash".into()),
            patterns: vec!["git status".into()],
            metadata: None,
            always: Vec::new(),
        }];
        let (app, platform) = build_app(cfg, backend).await;

        // Map the parent session to a chat WITHOUT an active accumulator (no
        // handle_message call — the turn is finished).
        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
                session_id: "ses_test".into(),
                directory: "/tmp/aa".into(),
                agent: None,
                auto_accept: false,
                topic_anchor: None,
            });
            store.persist().unwrap();
        }

        tokio::spawn({
            let app = app.clone();
            async move {
                let _ = app.permission.poll_loop(&app).await;
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(3500)).await;

        // No inline host: the permission becomes a separate card delivered into
        // the parent's chat (the child has no card of its own to host it).
        let calls = platform.calls.lock().await.clone();
        let perm_card = calls.iter().find_map(|c| match c {
            PlatformCall::SendCard { receive_id, card }
                if receive_id == "chat_1" && card.to_string().contains("git status") =>
            {
                Some(card.clone())
            }
            _ => None,
        });
        let perm_card =
            perm_card.expect("subtask permission should fall back to a separate card in the parent chat");
        // The card still carries the child's session id + owning directory.
        let first_button = perm_card["body"]["elements"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["tag"] == "button")
            .expect("permission card has buttons");
        let value = first_button["value"].clone();
        assert_eq!(value["session_id"], child);
        assert!(
            value["directory"]
                .as_str()
                .map(|d| !d.is_empty())
                .unwrap_or(false),
            "separate card must carry a directory, got: {}",
            value
        );
    }

    /// A topic-backed session with no streaming card falls back to a separate
    /// permission card. The card replies to the session's topic anchor (a
    /// message inside the topic), which keeps it inside the topic — the create
    /// API rejects `thread_id` as a receive target, so replying to the anchor is
    /// the reliable way to reach the topic.
    #[tokio::test]
    async fn separate_permission_card_sent_into_topic_for_topic_session() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut backend = MockBackend::new(realistic_parts());
        backend.permissions = vec![opencode::client::PermissionRequest {
            request_id: "per_topic".into(),
            session_id: Some("ses_topic".into()),
            permission: Some("bash".into()),
            patterns: vec!["cargo build".into()],
            metadata: None,
            always: Vec::new(),
        }];
        let (app, platform) = build_app(cfg, backend).await;

        // Map the session to a TOPIC (thread_id != chat_id) with an anchor
        // message inside the topic, no accumulator.
        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: crate::config::ThreadKey::new("chat_1".into(), "omt_topic_1".into()),
                session_id: "ses_topic".into(),
                directory: "/tmp/topic".into(),
                agent: None,
                auto_accept: false,
                topic_anchor: Some("msg_in_topic_anchor".into()),
            });
            store.persist().unwrap();
        }

        tokio::spawn({
            let app = app.clone();
            async move {
                let _ = app.permission.poll_loop(&app).await;
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(3500)).await;

        // The separate card must be reply'd to the topic anchor (not sent to
        // the chat top level).
        let calls = platform.calls.lock().await.clone();
        let perm_card = calls.iter().find_map(|c| match c {
            PlatformCall::ReplyCard { reply_to, card }
                if reply_to == "msg_in_topic_anchor" && card.to_string().contains("cargo build") =>
            {
                Some(card.clone())
            }
            _ => None,
        });
        assert!(
            perm_card.is_some(),
            "topic permission card should reply to the topic anchor, got: {calls:?}"
        );
        assert!(
            !calls
                .iter()
                .any(|c| matches!(c, PlatformCall::SendCard { receive_id, .. } if receive_id == "chat_1")),
            "permission card must NOT go to the chat top level: {calls:?}"
        );
    }

    /// A live E2E run: real cola bot (Platform) + a MOCK backend + the test bot
    /// reading the group.
    struct LiveHarness {
        app: Arc<App>,
        backend: Arc<MockBackend>,
        test_bot: feishu::Client,
        group_chat_id: String,
        _dir: tempfile::TempDir,
    }

    impl LiveHarness {
        /// Post a message into the group via the test bot and have cola process
        /// it; returns the sent message id (cola replies to it).
        async fn send_and_process(&self, prompt: &str) -> String {
            let sent_msg_id = self
                .test_bot
                .send_text("chat_id", &self.group_chat_id, prompt)
                .await
                .expect("send prompt to group");
            self.app
                .handle_message(
                    sent_msg_id.clone(),
                    self.group_chat_id.clone(),
                    "group".into(),
                    None,
                    prompt.to_string(),
                    None,
                )
                .await;
            sent_msg_id
        }

        /// Poll the group until an interactive card whose content contains
        /// `needle` appears; returns the content or "" on timeout.
        async fn wait_for_card(&self, needle: &str, timeout_secs: i64) -> String {
            let deadline = chrono::Utc::now() + chrono::Duration::seconds(timeout_secs);
            let mut found = String::new();
            loop {
                let msgs = self
                    .test_bot
                    .list_messages("chat", &self.group_chat_id)
                    .await
                    .expect("list group messages");
                for m in &msgs {
                    if m.msg_type == "interactive" {
                        let content = m
                            .body
                            .as_ref()
                            .and_then(|b| b.get("content"))
                            .and_then(|c| c.as_str())
                            .unwrap_or("");
                        if content.contains(needle) {
                            found = content.to_string();
                        }
                    }
                }
                if !found.is_empty() || chrono::Utc::now() > deadline {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(2000)).await;
            }
            found
        }
    }

    /// Shared live E2E setup. Returns None when the test-bot credentials aren't
    /// configured (the test then skips).
    async fn live_setup(backend: MockBackend) -> Option<LiveHarness> {
        let _ = tracing_subscriber::fmt()
            .with_env_filter("cola=debug")
            .with_writer(std::io::stderr)
            .try_init();

        #[derive(serde::Deserialize)]
        struct LiveTestCfg {
            #[serde(rename = "app_id")]
            app_id: String,
            #[serde(rename = "app_secret")]
            app_secret: String,
            #[serde(rename = "group_chat_id")]
            group_chat_id: String,
            #[serde(rename = "work_dir", default)]
            work_dir: Option<String>,
        }

        let test_cfg = std::fs::read_to_string("cola-test.toml")
            .ok()
            .and_then(|s| toml::from_str::<LiveTestCfg>(&s).ok());
        let test_app_id = test_cfg
            .as_ref()
            .map(|c| c.app_id.clone())
            .or_else(|| std::env::var("COLA_TEST_BOT_APP_ID").ok())
            .unwrap_or_default();
        let test_app_secret = test_cfg
            .as_ref()
            .map(|c| c.app_secret.clone())
            .or_else(|| std::env::var("COLA_TEST_BOT_APP_SECRET").ok())
            .unwrap_or_default();
        let group_chat_id = test_cfg
            .as_ref()
            .map(|c| c.group_chat_id.clone())
            .or_else(|| std::env::var("COLA_TEST_GROUP_CHAT_ID").ok())
            .unwrap_or_default();
        let work_dir = test_cfg
            .as_ref()
            .and_then(|c| c.work_dir.clone())
            .or_else(|| std::env::var("COLA_TEST_WORK_DIR").ok())
            .unwrap_or_default();
        if test_app_id.is_empty() || test_app_secret.is_empty() || group_chat_id.is_empty() {
            tracing::warn!("skipping live E2E: configure cola-test.toml or set the COLA_TEST_BOT_* env vars");
            return None;
        }

        // Load the cola bot config from the repo; point new sessions at the
        // configured work dir instead of chdir'ing the whole process.
        let mut cfg = crate::config::load(std::path::Path::new("cola.toml")).expect("load cola.toml");
        if !work_dir.is_empty() {
            cfg.bridge.work_dir = Some(work_dir.into());
        }
        let dir = tempfile::tempdir().unwrap();
        cfg.bridge.session_file = dir.path().join("sessions.json");

        let cola_platform = feishu::Client::new(cfg.feishu.clone());
        let test_bot = feishu::Client::new(crate::config::FeishuConfig {
            app_id: test_app_id,
            app_secret: test_app_secret,
        });
        let backend = Arc::new(backend);
        let app = Arc::new(App::new(cfg, backend.clone(), Arc::new(cola_platform)).unwrap());
        Some(LiveHarness {
            app,
            backend,
            test_bot,
            group_chat_id,
            _dir: dir,
        })
    }

    /// Live end-to-end wire check with a real Feishu bot.
    ///
    /// cola renders cards from a MOCK backend (deterministic, no real OpenCode
    /// server), the real cola bot posts them into a test group, and a second
    /// Feishu bot reads back what was actually delivered and asserts it matches
    /// expectation. This verifies the wire format Feishu accepts, not just the
    /// JSON cola builds in-process.
    ///
    /// Credentials come from `cola-test.toml` (gitignored, see the .example
    /// template) or the env vars COLA_TEST_BOT_APP_ID / COLA_TEST_BOT_APP_SECRET /
    /// COLA_TEST_GROUP_CHAT_ID. Run:
    ///   `cargo test --bin cola live_e2e_real_bot -- --ignored`
    #[tokio::test]
    #[ignore = "requires a second Feishu bot + a test group; see test docs"]
    async fn live_e2e_real_bot_renders_expected_cards() {
        let Some(harness) = live_setup(MockBackend::new(realistic_parts())).await else {
            return;
        };

        // The test bot sends a real message so cola has a real reply target.
        let prompt = "自动测试：请分析一下目录，然后汇报。";
        let _sent_msg_id = harness.send_and_process(prompt).await;

        // Read back the group until the cola bot's final Done card appears.
        // Feishu's start_time/end_time window returns empty for recent messages
        // on this API, so query without a window and filter client-side. Note:
        // the API only returns the v2 card's *fallback* (title + "upgrade your
        // client" placeholder), so this asserts real delivery + terminal header
        // state; the full reasoning/tool/text body is asserted in-process by
        // the RecordingPlatform tests. The needle is this test's own prompt so
        // it can't match a sibling live test sharing the group.
        let final_text = harness.wait_for_card("自动测试：请分析一下目录", 30).await;

        assert!(
            final_text.contains("✅"),
            "cola bot never posted a Done card to the group"
        );
        assert!(
            final_text.contains("自动测试：请分析一下目录"),
            "card fallback title should carry the question, got: {}",
            final_text
        );
    }

    /// Live E2E for the interactive `question` tool: the mock backend surfaces a
    /// pending question request, the question poller turns it into a real card
    /// the cola bot posts to the group, and the test bot reads it back.
    #[tokio::test]
    #[ignore = "requires a second Feishu bot + a test group; see live_e2e docs"]
    async fn live_e2e_question_card_is_delivered() {
        // Unique per run so wait_for_card can't match a stale question card left
        // in the group by a previous run.
        let marker = format!("q-{}", uuid::Uuid::new_v4().to_string().get(..8).unwrap_or("x"));
        let question_text = format!("你想继续吗？（{}）", marker);
        let mut backend = MockBackend::new(realistic_parts());
        backend.questions = vec![opencode::client::QuestionRequest {
            id: "que_live".into(),
            session_id: "ses_test".into(),
            questions: vec![opencode::client::QuestionInfo {
                question: question_text.clone(),
                header: "下一步".into(),
                options: vec![
                    opencode::client::QuestionOption {
                        label: "继续".into(),
                        description: String::new(),
                    },
                    opencode::client::QuestionOption {
                        label: "停止".into(),
                        description: String::new(),
                    },
                ],
                multiple: None,
                custom: None,
            }],
        }];
        let Some(harness) = live_setup(backend).await else {
            return;
        };

        // Give cola a session + reply target to work against.
        harness.send_and_process("自动测试：请回答我的问题。").await;

        // Run the question poller (it surfaces pending questions as cards).
        tokio::spawn({
            let app = harness.app.clone();
            async move {
                let _ = app.question.poll_loop(&app).await;
            }
        });

        let content = harness.wait_for_card(&marker, 30).await;
        assert!(
            content.contains("❓ AI 想问你"),
            "question card header missing: {}",
            content
        );
        assert!(
            content.contains(&question_text),
            "question card body missing: {}",
            content
        );
        assert!(
            content.contains("继续"),
            "question option button missing: {}",
            content
        );

        // Simulate the user clicking an option. Feishu's messages API strips
        // button `value` payloads from the returned card, so the click is driven
        // with the known payload (the payload shape is pinned in-process by the
        // Seam C card test); the wire test above already confirmed the card was
        // delivered with the question text and option labels.
        let value = serde_json::json!({
            "action": "question",
            "reply": "answer",
            "request_id": "que_live",
            "session_id": "ses_test",
            "question_index": 0,
            "answer": "继续",
        });
        let result = harness.app.handle_card_action(value).await;
        assert!(
            result.is_some(),
            "clicking an option should produce a result card"
        );

        let calls = harness.backend.reply_question_calls.lock().await.clone();
        assert!(
            calls
                .iter()
                .any(|(req, answers)| { req == "que_live" && answers == &vec![vec!["继续".to_string()]] }),
            "the chosen answer was not posted to the backend: {:?}",
            calls
        );
    }

    #[tokio::test]
    async fn new_session_uses_configured_work_dir() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = test_config(&dir.path().join("sessions.json"));
        let work = dir.path().join("work");
        cfg.bridge.work_dir = Some(work.clone());

        // No chdir: the session directory must come from [bridge] work_dir, not
        // the process cwd.
        let (app, _platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
        app.handle_message(
            "msg_1".into(),
            "chat_1".into(),
            "p2p".into(),
            None,
            "hi".into(),
            None,
        )
        .await;

        let thread = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        let entry = app
            .sessions
            .lock()
            .await
            .get_active(&thread)
            .cloned()
            .expect("a session should have been created");
        assert_eq!(entry.directory, work.to_string_lossy().to_string());
    }

    /// `/topic <dir>` creates a real Feishu topic (via reply_in_thread) backed
    /// by a new session rooted at <dir>, maps the returned thread_id to that
    /// session, and leaves the lobby conversation untouched.
    #[tokio::test]
    async fn topic_command_creates_topic_mapped_to_new_session() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut backend = MockBackend::new(realistic_parts());
        backend.session_id = "ses_topic".into();
        let title_calls = backend.update_title_calls.clone();
        let (app, platform) = build_app(cfg, backend).await;

        app.handle_command(
            Command::Topic {
                directory: "/root/proj/lib".into(),
                name: Some("api-refactor".into()),
            },
            crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            "msg_topic",
            crate::config::ConversationKind::P2p,
        )
        .await
        .unwrap();

        // The topic is created via reply_in_thread on the command message.
        let calls = platform.calls.lock().await.clone();
        assert!(
            calls.iter().any(
                |c| matches!(c, PlatformCall::ReplyInThread { message_id, .. } if message_id == "msg_topic")
            ),
            "expected a reply_in_thread on the command message, got {calls:?}"
        );

        // The created topic's thread_id is mapped to the new session.
        let topic_key = crate::config::ThreadKey::new("chat_1".into(), "omt_created_topic".into());
        let entry = app
            .sessions
            .lock()
            .await
            .get_active(&topic_key)
            .cloned()
            .expect("topic thread_id should map to the new session");
        assert_eq!(entry.session_id, "ses_topic");
        assert_eq!(entry.directory, "/root/proj/lib");
        // The named `/topic` PATCHed the server title (ADR-0007).
        assert_eq!(
            title_calls.lock().await.as_slice(),
            &[("ses_topic".to_string(), "api-refactor".to_string())]
        );
        // The topic anchor is the confirmation message INSIDE the topic; future
        // sent cards reply to it so they stay in the topic.
        assert_eq!(entry.topic_anchor.as_deref(), Some("msg_topic_reply"));

        // The lobby conversation still maps to nothing new (no session was
        // created for the lobby itself).
        let lobby_key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        assert!(app.sessions.lock().await.get_active(&lobby_key).is_none());
    }

    /// A message sent INSIDE the created topic routes to the topic's session,
    /// not to a fresh lobby session.
    #[tokio::test]
    async fn topic_command_created_topic_routes_messages_to_its_session() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut backend = MockBackend::new(realistic_parts());
        backend.session_id = "ses_topic".into();
        let prompt_calls = backend.prompt_calls.clone();
        let (app, _platform) = build_app(cfg, backend).await;

        // Create the topic first.
        app.handle_command(
            Command::Topic {
                directory: "/root/proj/lib".into(),
                name: None,
            },
            crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            "msg_topic",
            crate::config::ConversationKind::P2p,
        )
        .await
        .unwrap();

        // Now a message arrives inside that topic (thread_id = the mapped one).
        app.handle_message(
            "msg_in_topic".into(),
            "chat_1".into(),
            "p2p".into(),
            Some("omt_created_topic".into()),
            "帮我看看这个目录".into(),
            None,
        )
        .await;

        // It must reuse the topic session (ses_topic), not create a new one.
        let calls = prompt_calls.lock().await.clone();
        assert_eq!(calls, vec!["帮我看看这个目录".to_string()]);
        let store = app.sessions.lock().await;
        let topic_key = crate::config::ThreadKey::new("chat_1".into(), "omt_created_topic".into());
        assert_eq!(
            store.get_active(&topic_key).map(|e| e.session_id.as_str()),
            Some("ses_topic")
        );
        // The lobby got NO session of its own.
        let lobby_key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        assert!(store.get_active(&lobby_key).is_none());
    }

    /// `/topic` invoked from inside a topic is rejected with a note rather than
    /// nesting another topic.
    #[tokio::test]
    async fn topic_command_rejected_inside_existing_topic() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

        app.handle_command(
            Command::Topic {
                directory: "/root/proj/lib".into(),
                name: None,
            },
            crate::config::ThreadKey::new("chat_1".into(), "omt_existing".into()),
            "msg_topic",
            crate::config::ConversationKind::Topic,
        )
        .await
        .unwrap();

        // No session created, no topic created — just a plain text note.
        let calls = platform.calls.lock().await.clone();
        assert!(
            calls
                .iter()
                .all(|c| !matches!(c, PlatformCall::ReplyInThread { .. })),
            "must not create a topic from inside a topic: {calls:?}"
        );
        assert!(calls.iter().any(|c| matches!(c, PlatformCall::ReplyText { .. })));
        let store = app.sessions.lock().await;
        assert!(store.all_entries().is_empty());
    }

    #[tokio::test]
    async fn group_root_message_creates_lobby_session_and_shows_guidance() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

        // A top-level group message (no thread_id) is the group "lobby".
        app.handle_message(
            "msg_1".into(),
            "oc_group_1".into(),
            "group".into(),
            None,
            "hi".into(),
            None,
        )
        .await;

        // Guidance text is replied once.
        let calls = platform.calls.lock().await.clone();
        let guidance: Vec<_> = calls
            .iter()
            .filter_map(|c| match c {
                PlatformCall::ReplyText { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert!(
            guidance.iter().any(|t| t.contains("已创建群会话")),
            "expected lobby guidance, got: {:?}",
            guidance
        );

        // Session lives under the lobby key (chat_id == thread_id).
        let lobby_key = crate::config::ThreadKey::new("oc_group_1".into(), "oc_group_1".into());
        let entry = app
            .sessions
            .lock()
            .await
            .get_active(&lobby_key)
            .cloned()
            .expect("lobby session created");
        assert_eq!(entry.session_id, "ses_test");
    }

    #[tokio::test]
    async fn group_root_guidance_shown_once_per_lobby() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

        app.handle_message(
            "msg_1".into(),
            "oc_group_1".into(),
            "group".into(),
            None,
            "hi".into(),
            None,
        )
        .await;
        app.handle_message(
            "msg_2".into(),
            "oc_group_1".into(),
            "group".into(),
            None,
            "again".into(),
            None,
        )
        .await;

        let calls = platform.calls.lock().await.clone();
        let guidance: Vec<_> = calls
            .iter()
            .filter_map(|c| match c {
                PlatformCall::ReplyText { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            guidance.len(),
            1,
            "guidance must be one-time, got: {:?}",
            guidance
        );
    }

    #[tokio::test]
    async fn p2p_top_level_message_gets_no_guidance() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

        app.handle_message(
            "msg_1".into(),
            "oc_p2p_1".into(),
            "p2p".into(),
            None,
            "hi".into(),
            None,
        )
        .await;

        let calls = platform.calls.lock().await.clone();
        let guidance: Vec<_> = calls
            .iter()
            .filter_map(|c| match c {
                PlatformCall::ReplyText { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert!(
            guidance.is_empty(),
            "p2p must not show lobby guidance, got: {:?}",
            guidance
        );
    }

    #[tokio::test]
    async fn topic_message_isolates_session_from_lobby() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

        // Seed distinct sessions for the lobby key and the topic key.
        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: crate::config::ThreadKey::new("oc_group_1".into(), "oc_group_1".into()),
                session_id: "ses_lobby".into(),
                directory: "/tmp/lobby".into(),
                agent: None,
                auto_accept: false,
                topic_anchor: None,
            });
            store.set_active(crate::config::SessionEntry {
                thread_key: crate::config::ThreadKey::new("oc_group_1".into(), "omt_topic_1".into()),
                session_id: "ses_topic".into(),
                directory: "/tmp/topic".into(),
                agent: None,
                auto_accept: false,
                topic_anchor: None,
            });
            store.persist().unwrap();
        }

        // Lobby message routes to the lobby session; topic message routes to
        // the topic session — never creating or switching across.
        app.handle_message(
            "msg_1".into(),
            "oc_group_1".into(),
            "group".into(),
            None,
            "hi".into(),
            None,
        )
        .await;
        app.handle_message(
            "msg_2".into(),
            "oc_group_1".into(),
            "group".into(),
            Some("omt_topic_1".into()),
            "refactor".into(),
            None,
        )
        .await;

        let store = app.sessions.lock().await;
        let lobby = store
            .get_active(&crate::config::ThreadKey::new(
                "oc_group_1".into(),
                "oc_group_1".into(),
            ))
            .cloned()
            .unwrap();
        let topic = store
            .get_active(&crate::config::ThreadKey::new(
                "oc_group_1".into(),
                "omt_topic_1".into(),
            ))
            .cloned()
            .unwrap();
        assert_eq!(lobby.session_id, "ses_lobby");
        assert_eq!(topic.session_id, "ses_topic");
        assert_ne!(lobby.thread_key, topic.thread_key);
        drop(store);

        // No guidance: the lobby session already existed.
        let calls = platform.calls.lock().await.clone();
        let guidance: Vec<_> = calls
            .iter()
            .filter_map(|c| match c {
                PlatformCall::ReplyText { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert!(
            guidance.is_empty(),
            "no guidance when lobby exists, got: {:?}",
            guidance
        );
    }

    #[tokio::test]
    async fn p2p_topic_isolated_from_p2p_top_level() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let (app, _platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

        // Seed a p2p top-level session and a p2p topic session.
        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: crate::config::ThreadKey::new("oc_p2p_1".into(), "oc_p2p_1".into()),
                session_id: "ses_top".into(),
                directory: "/tmp/top".into(),
                agent: None,
                auto_accept: false,
                topic_anchor: None,
            });
            store.set_active(crate::config::SessionEntry {
                thread_key: crate::config::ThreadKey::new("oc_p2p_1".into(), "omt_p2p_1".into()),
                session_id: "ses_p2p_topic".into(),
                directory: "/tmp/ptopic".into(),
                agent: None,
                auto_accept: false,
                topic_anchor: None,
            });
            store.persist().unwrap();
        }

        app.handle_message(
            "msg_1".into(),
            "oc_p2p_1".into(),
            "p2p".into(),
            None,
            "hi".into(),
            None,
        )
        .await;
        app.handle_message(
            "msg_2".into(),
            "oc_p2p_1".into(),
            "p2p".into(),
            Some("omt_p2p_1".into()),
            "topic hi".into(),
            None,
        )
        .await;

        let store = app.sessions.lock().await;
        let top = store
            .get_active(&crate::config::ThreadKey::new(
                "oc_p2p_1".into(),
                "oc_p2p_1".into(),
            ))
            .cloned()
            .unwrap();
        let topic = store
            .get_active(&crate::config::ThreadKey::new(
                "oc_p2p_1".into(),
                "omt_p2p_1".into(),
            ))
            .cloned()
            .unwrap();
        assert_eq!(top.session_id, "ses_top");
        assert_eq!(topic.session_id, "ses_p2p_topic");
        assert_ne!(top.thread_key, topic.thread_key);
    }

    #[tokio::test]
    async fn stale_session_mapping_is_recreated_on_404() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut backend = MockBackend::new(realistic_parts());
        backend.session_id = "ses_new".into();
        backend.stale_session_404 = true;
        let (app, platform) = build_app(cfg, backend).await;

        // Seed stale mappings: thread -> ses_old (active), then ses_old2. When
        // ses_old 404s, cola must create a FRESH session, not fall through to
        // the next stale mapping.
        let thread = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: thread.clone(),
                session_id: "ses_old2".into(),
                directory: "/tmp/old2".into(),
                agent: None,
                auto_accept: false,
                topic_anchor: None,
            });
            store.set_active(crate::config::SessionEntry {
                thread_key: thread.clone(),
                session_id: "ses_old".into(),
                directory: "/tmp/old".into(),
                agent: None,
                auto_accept: false,
                topic_anchor: None,
            });
            store.persist().unwrap();
        }

        app.handle_message(
            "msg_1".into(),
            "chat_1".into(),
            "p2p".into(),
            None,
            "hi".into(),
            None,
        )
        .await;

        // The prompt on the stale session 404s; cola must recreate the session
        // and retry, landing on a Done card instead of an error.
        let calls = platform.calls.lock().await.clone();
        let updates: Vec<_> = calls
            .iter()
            .filter_map(|c| match c {
                PlatformCall::UpdateMessage { card, .. } => Some(card.clone()),
                _ => None,
            })
            .collect();
        assert!(!updates.is_empty(), "expected a card update, got: {:?}", calls);
        let card = updates.last().unwrap().to_string();
        assert!(card.contains("✅"), "expected a Done card, got: {}", card);

        // The store must now map the thread to the recreated session.
        let sid = app
            .sessions
            .lock()
            .await
            .get_active(&thread)
            .map(|e| e.session_id.clone());
        assert_eq!(sid.as_deref(), Some("ses_new"));
    }

    #[tokio::test]
    async fn question_card_action_posts_answer_back() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let backend = Arc::new(MockBackend::new(realistic_parts()));
        let platform = Arc::new(RecordingPlatform::new());
        let app = Arc::new(App::new(cfg, backend.clone(), platform).unwrap());

        // The poll loop has surfaced a pending question request.
        app.question.question_requests.lock().await.insert(
            "que_1".into(),
            opencode::client::QuestionRequest {
                id: "que_1".into(),
                session_id: "ses_1".into(),
                questions: vec![opencode::client::QuestionInfo {
                    question: "选择目录".into(),
                    header: "目录".into(),
                    options: vec![
                        opencode::client::QuestionOption {
                            label: "/a".into(),
                            description: String::new(),
                        },
                        opencode::client::QuestionOption {
                            label: "/b".into(),
                            description: String::new(),
                        },
                    ],
                    multiple: None,
                    custom: None,
                }],
            },
        );
        // Seed the session → directory mapping so the reply routes correctly.
        {
            let thread = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: thread,
                session_id: "ses_1".into(),
                directory: "/work".into(),
                agent: None,
                auto_accept: false,
                topic_anchor: None,
            });
        }

        // User clicks the "/a" option button.
        let value = serde_json::json!({
            "action": "question",
            "reply": "answer",
            "request_id": "que_1",
            "session_id": "ses_1",
            "question_index": 0,
            "answer": "/a",
        });
        let result = app.handle_card_action(value).await;
        assert!(result.is_some());
        assert!(
            result
                .unwrap()
                .card
                .as_ref()
                .unwrap()
                .to_string()
                .contains("已回答")
        );

        let calls = backend.reply_question_calls.lock().await.clone();
        assert_eq!(calls.len(), 1, "expected one reply_question call");
        assert_eq!(calls[0].0, "que_1");
        assert_eq!(calls[0].1, vec![vec!["/a".to_string()]]);
    }

    #[tokio::test]
    async fn question_card_action_rejects() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let backend = Arc::new(MockBackend::new(realistic_parts()));
        let platform = Arc::new(RecordingPlatform::new());
        let app = Arc::new(App::new(cfg, backend.clone(), platform).unwrap());

        let value = serde_json::json!({
            "action": "question",
            "reply": "reject",
            "request_id": "que_1",
            "session_id": "ses_1",
        });
        let result = app.handle_card_action(value).await;
        assert!(result.is_some());
        assert!(
            result
                .unwrap()
                .card
                .as_ref()
                .unwrap()
                .to_string()
                .contains("拒绝")
        );

        let calls = backend.reply_question_calls.lock().await.clone();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "que_1");
        assert!(calls[0].1[0][0].contains("reject"));
    }

    #[tokio::test]
    async fn double_click_on_same_request_replies_once() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let backend = Arc::new(MockBackend::new(realistic_parts()));
        let platform = Arc::new(RecordingPlatform::new());
        let app = Arc::new(App::new(cfg, backend.clone(), platform).unwrap());

        // A pending single-question request (the same one the card was built for).
        app.question.question_requests.lock().await.insert(
            "que_1".into(),
            opencode::client::QuestionRequest {
                id: "que_1".into(),
                session_id: "ses_1".into(),
                questions: vec![opencode::client::QuestionInfo {
                    question: "选择目录".into(),
                    header: "目录".into(),
                    options: vec![opencode::client::QuestionOption {
                        label: "/a".into(),
                        description: String::new(),
                    }],
                    multiple: None,
                    custom: None,
                }],
            },
        );

        let value = serde_json::json!({
            "action": "question",
            "reply": "answer",
            "request_id": "que_1",
            "session_id": "ses_1",
            "question_index": 0,
            "answer": "/a",
        });

        // First click replies; second (a fast re-click before the result card
        // replaces the buttons) must NOT re-reply — same request, one answer.
        let first = app.handle_card_action(value.clone()).await;
        assert!(first.is_some());
        let second = app.handle_card_action(value).await;
        assert!(second.is_some(), "second click still gets the result card");

        let calls = backend.reply_question_calls.lock().await.clone();
        assert_eq!(calls.len(), 1, "double click must not double-reply: {:?}", calls);
    }

    #[tokio::test]
    async fn question_with_multiple_parts_waits_for_all_answers() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let backend = Arc::new(MockBackend::new(realistic_parts()));
        let platform = Arc::new(RecordingPlatform::new());
        let app = Arc::new(App::new(cfg, backend.clone(), platform).unwrap());

        let mk_questions = || {
            vec![
                opencode::client::QuestionInfo {
                    question: "选择目录".into(),
                    header: "目录".into(),
                    options: vec![opencode::client::QuestionOption {
                        label: "/a".into(),
                        description: String::new(),
                    }],
                    multiple: None,
                    custom: None,
                },
                opencode::client::QuestionInfo {
                    question: "选择分支".into(),
                    header: "分支".into(),
                    options: vec![opencode::client::QuestionOption {
                        label: "main".into(),
                        description: String::new(),
                    }],
                    multiple: None,
                    custom: None,
                },
            ]
        };
        app.question.question_requests.lock().await.insert(
            "que_2".into(),
            opencode::client::QuestionRequest {
                id: "que_2".into(),
                session_id: "ses_1".into(),
                questions: mk_questions(),
            },
        );

        let value = |index: u64, answer: &str| {
            serde_json::json!({
                "action": "question",
                "reply": "answer",
                "request_id": "que_2",
                "session_id": "ses_1",
                "question_index": index,
                "answer": answer,
            })
        };

        // Answer the FIRST question only: must NOT submit (the second is open).
        let first = app.handle_card_action(value(0, "/a")).await;
        assert!(first.is_some());
        let first = first.unwrap();
        assert_eq!(first.toast.as_deref(), Some("已记录答案，还有 1 题未答"));
        // The returned card is still a question card (not a result card).
        let first_card = first.card.as_ref().unwrap().to_string();
        assert!(first_card.contains("❓ AI 想问你"));
        assert!(first_card.contains("已选：/a"));
        assert!(
            !first_card.contains("已选：main"),
            "question 2 not answered yet: {}",
            first_card
        );
        assert_eq!(backend.reply_question_calls.lock().await.len(), 0);

        // Answer the SECOND question: now everything is answered → submits.
        let second = app.handle_card_action(value(1, "main")).await;
        assert!(second.is_some());
        assert_eq!(second.unwrap().toast.as_deref(), Some("已回答"));
        let calls = backend.reply_question_calls.lock().await.clone();
        assert_eq!(calls.len(), 1, "one reply_question call total");
        assert_eq!(calls[0].0, "que_2");
        assert_eq!(calls[0].1, vec![vec!["/a".to_string()], vec!["main".to_string()]]);
    }

    /// A question raised during an active turn is surfaced INLINE on the
    /// streaming card; answering it only toasts (no card replacement) and the
    /// streaming card re-renders with the updated partial answers.
    #[tokio::test]
    async fn inline_question_answered_on_streaming_card() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut mock = MockBackend::new(realistic_parts());
        mock.questions = vec![opencode::client::QuestionRequest {
            id: "que_inline".into(),
            session_id: "ses_test".into(),
            questions: vec![
                opencode::client::QuestionInfo {
                    question: "选目录".into(),
                    header: "目录".into(),
                    options: vec![opencode::client::QuestionOption {
                        label: "/a".into(),
                        description: String::new(),
                    }],
                    multiple: None,
                    custom: None,
                },
                opencode::client::QuestionInfo {
                    question: "选分支".into(),
                    header: "分支".into(),
                    options: vec![opencode::client::QuestionOption {
                        label: "main".into(),
                        description: String::new(),
                    }],
                    multiple: None,
                    custom: None,
                },
            ],
        }];
        let backend = Arc::new(mock);
        let platform = Arc::new(RecordingPlatform::new());
        let app = Arc::new(App::new(cfg, backend.clone(), platform).unwrap());

        // Seed a session + active accumulator (an in-flight turn).
        app.handle_message(
            "msg_1".into(),
            "chat_1".into(),
            "p2p".into(),
            None,
            "hi".into(),
            None,
        )
        .await;
        assert!(
            app.accumulators.lock().await.contains_key("ses_test"),
            "accumulator expected"
        );

        // Run the question poller → the question is inlined on the accumulator.
        tokio::spawn({
            let app = app.clone();
            async move {
                let _ = app.question.poll_loop(&app).await;
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(3500)).await;

        let pending = app
            .accumulators
            .lock()
            .await
            .get("ses_test")
            .unwrap()
            .pending_questions
            .clone();
        assert_eq!(pending.len(), 1, "question should be inlined");
        assert_eq!(pending[0].request_id, "que_inline");

        // Answer the first question → toast only, no card replacement.
        let value = serde_json::json!({
            "action": "question",
            "reply": "answer",
            "request_id": "que_inline",
            "session_id": "ses_test",
            "question_index": 0,
            "answer": "/a",
        });
        let r1 = app.handle_card_action(value).await.expect("result");
        assert_eq!(r1.toast.as_deref(), Some("已记录答案，还有 1 题未答"));
        assert!(r1.card.is_none(), "inline answer must not replace the card");
        assert_eq!(backend.reply_question_calls.lock().await.len(), 0);
        // The accumulator's inline question reflects the partial answer.
        let pending = app
            .accumulators
            .lock()
            .await
            .get("ses_test")
            .unwrap()
            .pending_questions
            .clone();
        assert_eq!(pending[0].answers[0], Some(vec!["/a".to_string()]));
        assert_eq!(pending[0].answers[1], None);

        // Answer the second → finalized, reply called, inline section removed.
        let value = serde_json::json!({
            "action": "question",
            "reply": "answer",
            "request_id": "que_inline",
            "session_id": "ses_test",
            "question_index": 1,
            "answer": "main",
        });
        let r2 = app.handle_card_action(value).await.expect("result");
        assert_eq!(r2.toast.as_deref(), Some("已回答"));
        assert!(r2.card.is_none(), "inline final answer must not replace the card");
        let calls = backend.reply_question_calls.lock().await.clone();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].1, vec![vec!["/a".to_string()], vec!["main".to_string()]]);
        assert!(
            app.accumulators
                .lock()
                .await
                .get("ses_test")
                .unwrap()
                .pending_questions
                .is_empty()
        );
    }

    /// When a turn is already in flight, a new message must NOT start a
    /// competing run_prompt (which would overwrite the running accumulator and
    /// race on the same card). It goes through the supplement path: the message
    /// is sent fire-and-forget via prompt_async (OpenCode merges it into the
    /// current turn) and the user gets a notice — no Loading card, no second
    /// accumulator.
    #[tokio::test]
    async fn message_during_inflight_goes_to_supplement_path() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let backend = MockBackend::new(realistic_parts());
        let sup_calls = backend.prompt_async_calls.clone();
        let (app, platform) = build_app(cfg, backend).await;
        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
                session_id: "ses_test".into(),
                directory: "/tmp/aa".into(),
                agent: None,
                auto_accept: false,
                topic_anchor: None,
            });
            store.persist().unwrap();
        }
        app.inflight.lock().await.insert("ses_test".to_string());

        app.handle_message(
            "msg_sup".into(),
            "chat_1".into(),
            "p2p".into(),
            None,
            "补充一下，改用方案 B".into(),
            None,
        )
        .await;

        // prompt_async was called with the supplement text.
        let calls = sup_calls.lock().await.clone();
        assert!(
            calls.iter().any(|c| c.contains("补充一下，改用方案 B")),
            "supplement text must be sent via prompt_async: {:?}",
            calls
        );

        // NO Loading card / run_prompt was started for the supplement message.
        let sent = platform.calls.lock().await.clone();
        assert!(
            sent.iter().all(|c| matches!(c, PlatformCall::ReplyText { .. })),
            "supplement must only reply text, not start a card: {:?}",
            sent
        );
        // The in-flight marker is preserved (still running).
        assert!(app.inflight.lock().await.contains("ses_test"));
    }

    // ===== Session discovery & adoption (ADR-0008) =====

    /// A helper: a session in the shared store with the given title/dir/id.
    fn list_session(
        id: &str,
        title: &str,
        directory: &str,
        updated: i64,
    ) -> opencode::client::SessionListInfo {
        opencode::client::SessionListInfo {
            id: id.into(),
            title: title.into(),
            directory: directory.into(),
            parent_id: None,
            agent: None,
            model: None,
            time: Some(opencode::client::SessionTime {
                created: updated,
                updated,
                archived: None,
            }),
        }
    }

    #[tokio::test]
    async fn list_shows_global_sessions_marking_own() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut backend = MockBackend::new(realistic_parts());
        backend.session_list = vec![
            list_session("ses_alpha01", "外部会话", "/tmp/ext", 100),
            list_session("ses_beta02", "本地会话", "/work/cola", 300),
        ];
        let (app, platform) = build_app(cfg, backend).await;
        // Our own lobby session, so /list marks it as active/本会话.
        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
                session_id: "ses_beta02".into(),
                directory: "/work/cola".into(),
                agent: None,
                auto_accept: false,
                topic_anchor: None,
            });
        }

        app.handle_command(
            Command::List { keyword: None, all: false },
            crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            "msg_list",
            crate::config::ConversationKind::P2p,
        )
        .await
        .unwrap();

        let calls = platform.calls.lock().await.clone();
        let text = calls
            .iter()
            .filter_map(|c| match c {
                PlatformCall::ReplyText { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("外部会话"), "external session visible: {text}");
        assert!(text.contains("本地会话"), "own session visible: {text}");
        // The newest (updated 300) sorts first; own session marked active.
        let pos_ext = text.find("外部会话").unwrap();
        let pos_local = text.find("本地会话").unwrap();
        assert!(pos_local < pos_ext, "own (newer) session sorts first: {text}");
        assert!(text.contains("(active)") || text.contains("本会话"));
    }

    #[tokio::test]
    async fn list_filters_by_keyword_and_hides_children() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut backend = MockBackend::new(realistic_parts());
        backend.session_list = vec![
            list_session("ses_alpha01", "重写登录模块", "/work/auth", 100),
            list_session("ses_beta02", "修 bug", "/work/cola", 300),
            opencode::client::SessionListInfo {
                parent_id: Some("ses_alpha01".into()),
                ..list_session("ses_child09", "Child session - x", "/work/auth", 400)
            },
        ];
        let (app, platform) = build_app(cfg, backend).await;

        // Keyword filters by title.
        app.handle_command(
            Command::List { keyword: Some("登录".into()), all: false },
            crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            "msg_list",
            crate::config::ConversationKind::P2p,
        )
        .await
        .unwrap();
        let calls = platform.calls.lock().await.clone();
        let text = calls
            .iter()
            .filter_map(|c| match c {
                PlatformCall::ReplyText { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("重写登录模块"), "keyword match: {text}");
        assert!(!text.contains("修 bug"), "non-matching title filtered: {text}");

        // Without --all the child is hidden even though it is newest.
        platform.calls.lock().await.clear();
        app.handle_command(
            Command::List { keyword: None, all: false },
            crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            "msg_list2",
            crate::config::ConversationKind::P2p,
        )
        .await
        .unwrap();
        let calls = platform.calls.lock().await.clone();
        let text = calls
            .iter()
            .filter_map(|c| match c {
                PlatformCall::ReplyText { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!text.contains("Child session"), "child hidden by default: {text}");

        // --all reveals the child.
        platform.calls.lock().await.clear();
        app.handle_command(
            Command::List { keyword: None, all: true },
            crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            "msg_list3",
            crate::config::ConversationKind::P2p,
        )
        .await
        .unwrap();
        let calls = platform.calls.lock().await.clone();
        let text = calls
            .iter()
            .filter_map(|c| match c {
                PlatformCall::ReplyText { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("child09"), "child shown with --all: {text}");
    }

    /// Repeated `/list` within the 30 s TTL must not re-hit the server; an
    /// external rename is only visible after invalidation/expiry.
    #[tokio::test]
    async fn list_is_cached_within_ttl_and_invalidated_on_rename() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut backend = MockBackend::new(realistic_parts());
        backend.session_list = vec![list_session("ses_alpha01", "标题 A", "/work/a", 100)];
        let calls_counter = backend.list_sessions_calls.clone();
        let (app, _platform) = build_app(cfg, backend).await;
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: key.clone(),
                session_id: "ses_alpha01".into(),
                directory: "/work/a".into(),
                agent: None,
                auto_accept: false,
                topic_anchor: None,
            });
        }

        // Two /list in a row → one server fetch.
        app.handle_command(
            Command::List { keyword: None, all: false },
            key.clone(),
            "m1",
            crate::config::ConversationKind::P2p,
        )
        .await
        .unwrap();
        app.handle_command(
            Command::List { keyword: None, all: false },
            key.clone(),
            "m2",
            crate::config::ConversationKind::P2p,
        )
        .await
        .unwrap();
        assert_eq!(calls_counter.load(std::sync::atomic::Ordering::SeqCst), 1);

        // A rename invalidates the cache → next /list refetches.
        app.handle_command(
            Command::Name("新名字".into()),
            key.clone(),
            "m3",
            crate::config::ConversationKind::P2p,
        )
        .await
        .unwrap();
        app.handle_command(
            Command::List { keyword: None, all: false },
            key,
            "m4",
            crate::config::ConversationKind::P2p,
        )
        .await
        .unwrap();
        assert_eq!(calls_counter.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn attach_adopts_foreign_session_by_id() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut backend = MockBackend::new(realistic_parts());
        backend.session_list = vec![list_session(
            "ses_foreign123abc",
            "OpenChamber 里的任务",
            "/work/foreign",
            100,
        )];
        let (app, _platform) = build_app(cfg, backend).await;

        app.handle_command(
            Command::Attach { query: "ses_foreign123abc".into(), force: false },
            crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            "msg_attach",
            crate::config::ConversationKind::P2p,
        )
        .await
        .unwrap();

        // The thread now maps to the foreign session with its directory.
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        let entry = app.sessions.lock().await.get_active(&key).cloned().unwrap();
        assert_eq!(entry.session_id, "ses_foreign123abc");
        assert_eq!(entry.directory, "/work/foreign");
    }

    #[tokio::test]
    async fn attach_rejects_session_owned_by_another_thread_without_force() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut backend = MockBackend::new(realistic_parts());
        backend.session_list = vec![list_session(
            "ses_foreign123abc",
            "OpenChamber 里的任务",
            "/work/foreign",
            100,
        )];
        let mut platform = RecordingPlatform::new();
        platform.chat_names.insert("oc_group_other".into(), "隔壁群".into());
        let platform = Arc::new(platform);
        let app = Arc::new(App::new(cfg, Arc::new(backend), platform.clone()).unwrap());
        // Another thread already owns the session.
        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: crate::config::ThreadKey::new(
                    "oc_group_other".into(),
                    "oc_group_other".into(),
                ),
                session_id: "ses_foreign123abc".into(),
                directory: "/work/foreign".into(),
                agent: None,
                auto_accept: false,
                topic_anchor: None,
            });
        }

        app.handle_command(
            Command::Attach { query: "ses_foreign123abc".into(), force: false },
            crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            "msg_attach",
            crate::config::ConversationKind::P2p,
        )
        .await
        .unwrap();

        // Rejected: the current thread still has no session.
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        assert!(app.sessions.lock().await.get_active(&key).is_none());
        let calls = platform.calls.lock().await.clone();
        let text = calls
            .iter()
            .filter_map(|c| match c {
                PlatformCall::ReplyText { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("隔壁群"), "rejection names the owning chat: {text}");
        assert!(text.contains("--force"), "rejection points at --force: {text}");
    }

    #[tokio::test]
    async fn attach_force_steals_mapping_from_other_thread() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut backend = MockBackend::new(realistic_parts());
        backend.session_list = vec![list_session(
            "ses_foreign123abc",
            "OpenChamber 里的任务",
            "/work/foreign",
            100,
        )];
        let (app, _platform) = build_app(cfg, backend).await;
        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: crate::config::ThreadKey::new(
                    "oc_group_other".into(),
                    "oc_group_other".into(),
                ),
                session_id: "ses_foreign123abc".into(),
                directory: "/work/foreign".into(),
                agent: None,
                auto_accept: false,
                topic_anchor: None,
            });
        }

        app.handle_command(
            Command::Attach { query: "ses_foreign123abc".into(), force: true },
            crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            "msg_attach",
            crate::config::ConversationKind::P2p,
        )
        .await
        .unwrap();

        // Stolen: current thread owns it, other thread is sessionless.
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        assert_eq!(
            app.sessions.lock().await.get_active(&key).unwrap().session_id,
            "ses_foreign123abc"
        );
        let other = crate::config::ThreadKey::new("oc_group_other".into(), "oc_group_other".into());
        assert!(app.sessions.lock().await.get_active(&other).is_none());
    }

    #[tokio::test]
    async fn forget_unmaps_thread_keeping_server_session() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let (app, _platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
                session_id: "ses_test".into(),
                directory: "/tmp/aa".into(),
                agent: None,
                auto_accept: false,
                topic_anchor: None,
            });
        }

        app.handle_command(
            Command::Forget,
            crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            "msg_forget",
            crate::config::ConversationKind::P2p,
        )
        .await
        .unwrap();

        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        assert!(app.sessions.lock().await.get_active(&key).is_none());
    }

    #[tokio::test]
    async fn switch_adopts_unique_foreign_session() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut backend = MockBackend::new(realistic_parts());
        backend.session_list = vec![list_session(
            "ses_alpha01",
            "唯一外部标题",
            "/work/ext",
            100,
        )];
        let (app, _platform) = build_app(cfg, backend).await;

        app.handle_command(
            Command::Switch("唯一外部标题".into()),
            crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            "msg_switch",
            crate::config::ConversationKind::P2p,
        )
        .await
        .unwrap();

        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        let entry = app.sessions.lock().await.get_active(&key).cloned().unwrap();
        assert_eq!(entry.session_id, "ses_alpha01");
        assert_eq!(entry.directory, "/work/ext");
    }

    #[tokio::test]
    async fn switch_ambiguous_global_match_lists_candidates() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut backend = MockBackend::new(realistic_parts());
        backend.session_list = vec![
            list_session("ses_alpha01", "任务 A", "/work/a", 100),
            list_session("ses_beta02", "任务 B", "/work/b", 200),
        ];
        let (app, platform) = build_app(cfg, backend).await;

        app.handle_command(
            Command::Switch("任务".into()),
            crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            "msg_switch",
            crate::config::ConversationKind::P2p,
        )
        .await
        .unwrap();

        // Ambiguous → no adoption, candidates listed.
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        assert!(app.sessions.lock().await.get_active(&key).is_none());
        let calls = platform.calls.lock().await.clone();
        let text = calls
            .iter()
            .filter_map(|c| match c {
                PlatformCall::ReplyText { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("/attach"), "points at /attach: {text}");
    }

    #[tokio::test]
    async fn switch_prefers_threads_own_sessions() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut backend = MockBackend::new(realistic_parts());
        backend.session_list = vec![
            list_session("ses_own1", "本项目会话", "/work/cola", 500),
            list_session("ses_foreign", "本项目会话", "/other/place", 100),
        ];
        let (app, _platform) = build_app(cfg, backend).await;
        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
                session_id: "ses_own1".into(),
                directory: "/work/cola".into(),
                agent: None,
                auto_accept: false,
                topic_anchor: None,
            });
            store.set_active(crate::config::SessionEntry {
                thread_key: crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
                session_id: "ses_other_own".into(),
                directory: "/work/other".into(),
                agent: None,
                auto_accept: false,
                topic_anchor: None,
            });
        }

        app.handle_command(
            Command::Switch("本项目".into()),
            crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            "msg_switch",
            crate::config::ConversationKind::P2p,
        )
        .await
        .unwrap();

        // The thread's own session wins (mapping unchanged, just active).
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        assert_eq!(
            app.sessions.lock().await.get_active(&key).unwrap().session_id,
            "ses_own1"
        );
    }

    #[tokio::test]
    async fn name_patches_server_title() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let backend = MockBackend::new(realistic_parts());
        let title_calls = backend.update_title_calls.clone();
        let (app, _platform) = build_app(cfg, backend).await;
        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
                session_id: "ses_test".into(),
                directory: "/tmp/aa".into(),
                agent: None,
                auto_accept: false,
                topic_anchor: None,
            });
        }

        app.handle_command(
            Command::Name("新名字".into()),
            crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            "msg_name",
            crate::config::ConversationKind::P2p,
        )
        .await
        .unwrap();

        assert_eq!(
            title_calls.lock().await.as_slice(),
            &[("ses_test".to_string(), "新名字".to_string())]
        );
    }

    #[tokio::test]
    async fn topic_with_session_rejects_selection_commands() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut backend = MockBackend::new(realistic_parts());
        backend.session_list = vec![list_session(
            "ses_foreign123abc",
            "外部会话",
            "/work/ext",
            100,
        )];
        let (app, platform) = build_app(cfg, backend).await;
        // The topic already owns a session.
        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: crate::config::ThreadKey::new("chat_1".into(), "omt_t_1".into()),
                session_id: "ses_topic_owned".into(),
                directory: "/work/topic".into(),
                agent: None,
                auto_accept: false,
                topic_anchor: Some("msg_anchor".into()),
            });
        }
        let topic_key = crate::config::ThreadKey::new("chat_1".into(), "omt_t_1".into());

        for cmd in [
            Command::List { keyword: None, all: false },
            Command::Switch("外部".into()),
            Command::Attach { query: "ses_foreign123abc".into(), force: false },
            Command::New(None),
            Command::Dir("/work/x".into()),
        ] {
            platform.calls.lock().await.clear();
            app.handle_command(cmd.clone(), topic_key.clone(), "msg_topic", crate::config::ConversationKind::Topic)
                .await
                .unwrap();
            let calls = platform.calls.lock().await.clone();
            let text = calls
                .iter()
                .filter_map(|c| match c {
                    PlatformCall::ReplyText { text, .. } => Some(text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                text.contains("回主对话操作"),
                "{cmd:?} must be rejected in a bound topic: {text}"
            );
        }
        // The topic's session mapping is untouched.
        assert_eq!(
            app.sessions.lock().await.get_active(&topic_key).unwrap().session_id,
            "ses_topic_owned"
        );
    }

    #[tokio::test]
    async fn fresh_topic_attach_adopts_with_in_topic_anchor() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut backend = MockBackend::new(realistic_parts());
        backend.session_list = vec![list_session(
            "ses_foreign123abc",
            "外部会话",
            "/work/ext",
            100,
        )];
        let (app, _platform) = build_app(cfg, backend).await;
        let topic_key = crate::config::ThreadKey::new("chat_1".into(), "omt_fresh".into());

        app.handle_command(
            Command::Attach { query: "ses_foreign123abc".into(), force: false },
            topic_key.clone(),
            "msg_topic_cmd",
            crate::config::ConversationKind::Topic,
        )
        .await
        .unwrap();

        // Adopted as the topic's single session, anchored to a reply inside it.
        let entry = app.sessions.lock().await.get_active(&topic_key).cloned().unwrap();
        assert_eq!(entry.session_id, "ses_foreign123abc");
        assert_eq!(entry.topic_anchor.as_deref(), Some("msg_topic_reply"));
    }
}
