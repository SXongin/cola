use std::ops::Deref;
use std::sync::Arc;

use crate::bridge::command;
use crate::bridge::core::SharedCore;
use crate::bridge::turn::PromptContext;
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
pub(crate) fn image_inputs(
    images: &[crate::feishu::client::ImageAttachment],
) -> Vec<opencode::client::ImageInput> {
    use base64::Engine;
    images
        .iter()
        .map(|img| opencode::client::ImageInput {
            mime: img.mime.clone(),
            data_base64: base64::engine::general_purpose::STANDARD.encode(&img.data),
        })
        .collect()
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
#[derive(Clone)]
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

/// Nested-topic rejection (ADR-0006, ADR-0025): a topic can only be created
/// from a non-topic message. A card whose `thread_id != chat_id` lives inside
/// a topic — a never-bound topic may open the `/dir`/`/switch` cards to bind
/// its single session, but the topic-creating ops must not nest. The one
/// guard + message shared by both card ops keeps their copies from drifting
/// (the text forms carry the same rule with their own reply text).
const NESTED_TOPIC_REJECTION: &str = "话题里不能开话题，请回主对话操作。";

fn reject_nested_topic(thread_key: &ThreadKey) -> Option<CardActionResult> {
    (thread_key.thread_id != thread_key.chat_id).then(|| CardActionResult {
        card: None,
        toast: Some(NESTED_TOPIC_REJECTION.to_string()),
    })
}

/// A display label for the Chat/Topic that owns a session, for the force-confirm
/// card: the chat's display name (falling back to its id), with `（话题）` when
/// the owner is a Topic rather than the Chat lobby. Vocabulary per CONTEXT.md —
/// the Feishu side is a Chat/Topic, never a "conversation".
async fn owner_label(core: &Arc<SharedCore>, owner_key: &ThreadKey) -> String {
    let chat_name = core
        .feishu
        .chat_name(&owner_key.chat_id)
        .await
        .unwrap_or(None)
        .unwrap_or_else(|| owner_key.chat_id.clone());
    if owner_key.thread_id != owner_key.chat_id {
        format!("{chat_name}（话题）")
    } else {
        chat_name
    }
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
            core,
        })
    }

    /// Recover an `Arc<Self>` from the weak self-reference set by `run`. Used
    /// by the `EventSink` trait impl to hand a `&Arc<App>` to the inherent
    /// methods. None before `run` sets it (and after it returns).
    fn self_arc(&self) -> Option<Arc<Self>> {
        self.self_weak.get().and_then(|w| w.upgrade())
    }

    /// Announce a completed `/restart` or `/update` from the payload the dying
    /// process left at `command::restart_notify_path`. A command issued inside
    /// a Topic is announced INSIDE that topic by replying to its command
    /// message — the create API rejects `receive_id_type=thread_id`, and a
    /// reply to an in-topic message stays in the topic (ADR-0006). A lobby
    /// command gets the chat-level card. A failed topic reply falls back to
    /// the chat so the announcement is never silently lost.
    pub(crate) async fn announce_restart(&self, notify: &command::RestartNotify) {
        let chat_id = notify.chat_id.as_str();
        let (title, body) = match notify.kind {
            command::RestartKind::Update => {
                let version = notify.version.as_deref().unwrap_or("");
                (
                    format!("✅ 已更新到 {version}"),
                    format!("cola 已更新到 {version} 并重启完成。"),
                )
            }
            command::RestartKind::Restart => ("♻️ 已重启".to_string(), "cola 已重启完成。".to_string()),
        };
        let card = crate::feishu::card::card_shell(
            &title,
            "green",
            vec![serde_json::json!({ "tag": "markdown", "content": body })],
        );
        // A topic command carries its `thread_id`; the command message itself
        // lives inside the topic, so replying to it keeps the card there.
        let topic_reply_to = notify
            .thread_id
            .as_deref()
            .filter(|t| !t.is_empty() && *t != chat_id)
            .and(notify.message_id.as_deref());
        let sent = match topic_reply_to {
            Some(message_id) => match self.feishu.reply_card(message_id, &card).await {
                Ok(_) => Ok(()),
                Err(e) => {
                    tracing::warn!("restart announce in topic failed: {e}; announcing in chat");
                    self.feishu.send_card("chat_id", chat_id, &card).await.map(|_| ())
                }
            },
            None => self.feishu.send_card("chat_id", chat_id, &card).await.map(|_| ()),
        };
        match sent {
            Ok(()) => tracing::info!("announced restart in chat {}", chat_id),
            Err(e) => tracing::warn!("restart announce failed: {}", e),
        }
    }

    pub async fn run(self: Arc<Self>) -> anyhow::Result<()> {
        // Let the EventSink trait impl recover a &Arc<App> from &self.
        let _ = self.self_weak.set(Arc::downgrade(&self));
        // After a `/restart` or self-update, announce it where the command was
        // issued: inside the Topic when there was one, else in the chat.
        let notify_path = command::restart_notify_path();
        if let Ok(raw) = std::fs::read_to_string(&notify_path)
            && let Ok(notify) = serde_json::from_str::<command::RestartNotify>(&raw)
        {
            let _ = std::fs::remove_file(&notify_path);
            self.announce_restart(&notify).await;
        }

        // Silent startup self-update check (ADR-0015): log when a new version
        // exists; never sends a card. Fire-and-forget — a dead network or a
        // rate limit only costs a debug log line.
        {
            tokio::spawn(async move {
                match crate::update::check().await {
                    Ok(crate::update::UpdateCheck::Available(info)) => {
                        if crate::version::is_dev_build() {
                            // A dev build is typically ahead of the last release
                            // (ADR-0027): warn instead of implying an upgrade.
                            tracing::warn!(
                                "本地 dev 构建（{}）发现新版本 {} —— 自更新会替换为发布版，可能比本地代码旧。",
                                crate::version::display_version(),
                                info.latest
                            );
                        } else {
                            tracing::info!(
                                "新版本 {} 可用（当前 {}）—— 发送 /update 更新。",
                                info.latest,
                                info.current
                            );
                        }
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
        //
        // ADR-0023: never inject the topic's own creation messages — the
        // thread root (the user's `/topic` command, `topic_root`) and the seed
        // card (`topic_anchor`). Feishu reports a plain topic reply's parent_id
        // pointing at the root, so without this guard every prompt in a
        // cola-created topic would carry boilerplate. Manually-created topics
        // leave both `None`, so their user-typed root still injects.
        let parent_is_topic_creation = match parent_id.as_deref() {
            Some(pid) => self
                .sessions
                .lock()
                .await
                .get_active(&thread_key)
                .is_some_and(|e| {
                    e.topic_root.as_deref() == Some(pid) || e.topic_anchor.as_deref() == Some(pid)
                }),
            None => false,
        };
        if let Some(pid) = parent_id.as_deref().filter(|_| !parent_is_topic_creation) {
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
                // Each supplement is its own logical user message: fresh
                // cola-authored id (ADR-0026), never reused.
                let cola_msg_id = crate::opencode::client::cola_message_id();
                match self
                    .opencode
                    .prompt_async(
                        &session_id,
                        &text,
                        &image_inputs,
                        self.session_model_override(&session_id).await.as_ref(),
                        self.session_variant_override(&session_id).await.as_deref(),
                        self.session_agent_override(&session_id).await.as_deref(),
                        Some(&cola_msg_id),
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
            cola_message_id: None,
            images,
        })
        .await
    }

    /// Run one prompt end-to-end: show a Loading card (either a fresh reply or a
    /// reset of an existing card when retrying), stream parts via the poll loop,
    /// then render the final Done/Error card. Shared by fresh messages and the
    /// error-card "retry" action.
    async fn run_prompt(self: &Arc<Self>, ctx: PromptContext) -> crate::error::Result<()> {
        crate::bridge::turn::Turn::run(self, ctx).await
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
        let id = self
            .create_fresh_session(thread_key, text, directory, None, None)
            .await?;
        Ok((id, true))
    }

    /// Create a brand-new session on the current server and make it the active
    /// one for the thread. Used when a mapped session no longer exists (404).
    /// The per-session overrides (agent/model/variant/auto_accept) reset to
    /// defaults, but the topic's creation messages (`topic_anchor`/`topic_root`,
    /// ADR-0023) are Feishu message ids — not session state — and survive so the
    /// quote-injection guard keeps working after the recreate.
    pub(crate) async fn create_fresh_session(
        &self,
        thread_key: &ThreadKey,
        _text: &str,
        directory: String,
        topic_anchor: Option<String>,
        topic_root: Option<String>,
    ) -> crate::error::Result<String> {
        let session = self
            .opencode
            .create_session(&self.opencode.new_session_input(Some(&directory)))
            .await?;
        let mut entry = SessionEntry::new(thread_key.clone(), session.id.clone(), directory);
        entry.topic_anchor = topic_anchor;
        entry.topic_root = topic_root;
        self.activate_session(entry).await?;
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
            "dir" => self.handle_dir_card_action(&self.core, &value).await,
            "agent" => self.handle_agent_card_action(&self.core, &value, false).await,
            "agent_clear" => self.handle_agent_card_action(&self.core, &value, true).await,
            "model" => self.handle_model_card_action(&self.core, &value).await,
            "think" => self.handle_think_card_action(&self.core, &value, false).await,
            "think_clear" => self.handle_think_card_action(&self.core, &value, true).await,
            "autoaccept" => self.handle_autoaccept_card_action(&self.core, &value).await,
            _ => None,
        }
    }

    /// Handle a `/switch` card button (ADR-0012, issue 04): adopt a session,
    /// create a new one, or re-search. The 接管/切换 ops return the Session
    /// Snapshot (or the compact suppressed-切换 state, ADR-0028) as the card
    /// so the ack patches the switch card in place — one message per
    /// activation; scope/search/new return a refreshed list card. Plus a
    /// Toast for instant feedback.
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
            .trim()
            .to_string();
        let scope = crate::bridge::command::SwitchScope::parse(
            value.get("scope").and_then(|v| v.as_str()).unwrap_or(""),
        );

        match op {
            "scope" => {
                // The 全部/本目录 toggle (ADR-0022): the button carries the
                // TARGET scope ("all"/"dir"); the outer `scope` above already
                // parsed it. Rebuild the card in that scope, keeping the
                // keyword so a scoped search survives the toggle.
                Some(CardActionResult {
                    card: Some(
                        self.build_switch_card_for(core, &thread_key, &keyword, scope)
                            .await,
                    ),
                    toast: None,
                })
            }
            "adopt" | "force_adopt" => {
                // Adopt/switch the target session into this thread. The plain
                // `adopt` op carries no `--force`: an occupied session patches
                // the card to the force-confirm card, whose `force_adopt`
                // button re-enters here. `force_adopt` skips the owner check and
                // lets the adoption's `set_active` steal the mapping.
                let force = op == "force_adopt";
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
                        card: Some(
                            self.build_switch_card_for(core, &thread_key, &keyword, scope)
                                .await,
                        ),
                        // The target session is already this conversation's
                        // active session — nothing changed. 会话 here names the
                        // OpenCode session (ADR-0022 keeps it), not the Feishu
                        // side.
                        toast: Some("已在当前会话".to_string()),
                    });
                }
                if !force {
                    let owner = {
                        let store = core.sessions.lock().await;
                        store.thread_for_session(&target.id)
                    };
                    if let Some(owner_key) = owner
                        && owner_key != thread_key
                    {
                        let owner_label = owner_label(core, &owner_key).await;
                        return Some(CardActionResult {
                            card: Some(crate::feishu::card::build_force_confirm_card(
                                &thread_key,
                                &target,
                                &owner_label,
                                "force_adopt",
                                "强制接管",
                                scope,
                            )),
                            toast: Some(format!("该会话被 {} 占用，请确认是否强制接管", owner_label)),
                        });
                    }
                }
                // ADR-0028 one-card rule: the switch card's OWN message becomes
                // the confirmation — the ack patches the list card in place to
                // the snapshot.
                let open_message_id = value
                    .get("open_message_id")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                Some(
                    self.apply_card_adoption(core, &thread_key, &target, open_message_id)
                        .await,
                )
            }
            "back" => Some(CardActionResult {
                card: Some(
                    self.build_switch_card_for(core, &thread_key, &keyword, scope)
                        .await,
                ),
                toast: None,
            }),
            "new" => {
                // Fresh session in the current project (equivalent to `/new`).
                let directory = core.current_project_directory(&thread_key).await;
                match core
                    .opencode
                    .create_session(&core.opencode.new_session_input(Some(&directory)))
                    .await
                {
                    Ok(session) => {
                        let entry = crate::config::SessionEntry::new(
                            thread_key.clone(),
                            session.id.clone(),
                            directory,
                        );
                        if let Err(e) = core.activate_session(entry).await {
                            tracing::warn!("switch card new: persist failed: {}", e);
                        }
                        Some(CardActionResult {
                            card: Some(self.build_switch_card_for(core, &thread_key, "", scope).await),
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
                    card: Some(
                        self.build_switch_card_for(core, &thread_key, &keyword, scope)
                            .await,
                    ),
                    toast: None,
                })
            }
            "topic_adopt" | "force_topic_adopt" => {
                // "建话题接管" (ADR-0016): open a NEW Feishu topic around an
                // existing session, in one gesture from the session card. The
                // topic anchors on the card's own message (`open_message_id`,
                // threaded through by `extract_card_action_value`), so fallback
                // cards can reply inside it. The plain `topic_adopt` op patches
                // an occupied session to the force-confirm card (强制建话题接管),
                // whose `force_topic_adopt` button re-enters here and skips the
                // owner check.
                // A topic can only be created from a non-topic message
                // (ADR-0006, ADR-0025): the session card may legally open in a
                // never-bound topic, but its topic op must not nest there —
                // same guard as the `/dir` card's "建话题" op and the text
                // `/topic --adopt` forms.
                let force = op == "force_topic_adopt";
                if let Some(rejection) = reject_nested_topic(&thread_key) {
                    return Some(rejection);
                }
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
                if !force {
                    let owner = {
                        let store = core.sessions.lock().await;
                        store.thread_for_session(&target.id)
                    };
                    if let Some(owner_key) = owner
                        && owner_key != thread_key
                    {
                        let owner_label = owner_label(core, &owner_key).await;
                        return Some(CardActionResult {
                            card: Some(crate::feishu::card::build_force_confirm_card(
                                &thread_key,
                                &target,
                                &owner_label,
                                "force_topic_adopt",
                                "强制建话题接管",
                                scope,
                            )),
                            toast: Some(format!("会话被 {} 占用，请确认是否强制接管", owner_label)),
                        });
                    }
                }
                Some(
                    self.topic_adopt_target(core, &thread_key, &target, &open_message_id, &keyword, scope)
                        .await,
                )
            }
            _ => None,
        }
    }

    /// Rebuild the `/switch` card for a thread with the given search keyword
    /// and list scope (ADR-0022).
    async fn build_switch_card_for(
        self: &Arc<Self>,
        core: &Arc<SharedCore>,
        thread_key: &ThreadKey,
        keyword: &str,
        scope: crate::bridge::command::SwitchScope,
    ) -> serde_json::Value {
        let (shown, active_id, mapped_ids, scope, current_dir) =
            crate::bridge::command::switch_card_data(core, thread_key, keyword, scope).await;
        crate::feishu::card::build_switch_card(
            thread_key,
            &shown,
            keyword,
            scope,
            current_dir.as_deref(),
            active_id.as_deref(),
            &mapped_ids,
        )
    }

    /// The adoption tail shared by the card's 接管/切换 and 强制接管 ops: build
    /// the Session Snapshot (or the compact suppressed-切换 state card,
    /// ADR-0028), persist the new active entry, and settle the snapshot's
    /// claimed pendings. The returned card patches the clicked card in place
    /// (ADR-0028 one-card rule), so no second message is sent.
    ///
    /// The steal is `set_active`'s: it removes any existing entry with this
    /// session id, so a `force_adopt` from another thread leaves that owner
    /// sessionless there. No pre-remove — if the snapshot work failed, the old
    /// mapping would otherwise be lost for nothing.
    async fn apply_card_adoption(
        &self,
        core: &Arc<SharedCore>,
        thread_key: &ThreadKey,
        target: &crate::opencode::SessionListInfo,
        open_message_id: Option<String>,
    ) -> CardActionResult {
        let mapped_to_this_thread = {
            let store = core.sessions.lock().await;
            store.thread_for_session(&target.id).as_ref() == Some(thread_key)
        };
        let verb = if mapped_to_this_thread { "切换" } else { "接管" };
        // `claim_data` is the snapshot's claimable pendings: claimed against
        // the patched card once the ack result is assembled, so the poll loop
        // never duplicates the embedded blocks.
        let (card, claim_data) = if mapped_to_this_thread {
            let data =
                crate::bridge::snapshot::gather_snapshot(&core.opencode, &target.id, &target.directory).await;
            match crate::bridge::snapshot::re_switch_emit(&data) {
                crate::bridge::snapshot::SnapshotEmit::Full => {
                    let (card, data) =
                        crate::bridge::command::snapshot_card_from_data(core, "切换", &target.title, data)
                            .await;
                    (card, Some(data))
                }
                crate::bridge::snapshot::SnapshotEmit::Suppressed => (
                    crate::feishu::snapshot_card::build_switched_state_card(
                        &target.title,
                        &target.id,
                        &target.directory,
                    ),
                    None,
                ),
            }
        } else {
            let (card, data) = crate::bridge::command::snapshot_card_for(core, "接管", target).await;
            (card, Some(data))
        };
        // In a topic the patched card lives INSIDE it, so persist its own
        // message id as the fallback-card anchor (same anchor semantics as the
        // text in-topic adopt, ADR-0028): later permission/question cards reply
        // to it and stay in the topic.
        let topic_anchor = if thread_key.thread_id != thread_key.chat_id {
            open_message_id.clone()
        } else {
            None
        };
        let mut entry =
            crate::config::SessionEntry::new(thread_key.clone(), target.id.clone(), target.directory.clone());
        entry.agent = target.agent.clone();
        entry.topic_anchor = topic_anchor;
        if let Err(e) = core.activate_session(entry).await {
            tracing::warn!("switch card adopt: persist failed: {}", e);
        }
        if let (Some(message_id), Some(data)) = (&open_message_id, &claim_data) {
            crate::bridge::external::settle_snapshot_after_send(core, message_id, verb, &target.title, data)
                .await;
        }
        CardActionResult {
            card: Some(card),
            toast: Some(format!(
                "已{verb}「{}」",
                crate::bridge::command::title_or_id_tail(target)
            )),
        }
    }

    /// The topic-creation tail shared by the card's 建话题接管 and
    /// 强制建话题接管 ops: open a topic anchored on the card message and map
    /// the adopted session to the new topic's `ThreadKey` (shared with the text
    /// `/topic --adopt` form, ADR-0016), then refresh the list card in place.
    async fn topic_adopt_target(
        self: &Arc<Self>,
        core: &Arc<SharedCore>,
        thread_key: &ThreadKey,
        target: &crate::opencode::SessionListInfo,
        open_message_id: &str,
        keyword: &str,
        scope: crate::bridge::command::SwitchScope,
    ) -> CardActionResult {
        let new_thread_id = match crate::bridge::command::create_topic_and_map_adopted(
            core,
            thread_key,
            target,
            open_message_id,
        )
        .await
        {
            Ok(Some(id)) => id,
            Ok(None) => {
                return CardActionResult {
                    card: None,
                    toast: Some("创建话题失败（未返回 thread_id）".to_string()),
                };
            }
            Err(e) => {
                tracing::warn!("switch card topic_adopt: create topic failed: {}", e);
                return CardActionResult {
                    card: None,
                    toast: Some("创建话题失败".to_string()),
                };
            }
        };
        tracing::info!(
            "switch card topic_adopt: created topic {} for session {} in chat {}",
            new_thread_id,
            target.id,
            thread_key.chat_id
        );
        CardActionResult {
            card: Some(self.build_switch_card_for(core, thread_key, keyword, scope).await),
            toast: Some("已建话题接管".to_string()),
        }
    }

    /// Rebuild the `/dir` Recent Directories card for a thread.
    async fn build_dir_card_for(
        self: &Arc<Self>,
        core: &Arc<SharedCore>,
        thread_key: &ThreadKey,
    ) -> serde_json::Value {
        let (dirs, current_dir) = crate::bridge::command::dir_card_data(core, thread_key).await;
        crate::feishu::card::build_dir_card(thread_key, &dirs, current_dir.as_deref())
    }

    /// Handle a `/dir` Recent Directories card button (ADR-0025): `op:
    /// "pick"` re-roots the thread into the picked directory by creating a
    /// NEW session there (matching the text `/dir <path>` form); `op:
    /// "topic"` wraps a NEW session in a brand-new Feishu topic instead — the
    /// card equivalent of `/topic <dir>`. Clicking the current directory's
    /// `pick` is a no-op that just toasts. Refreshes the card in place so the
    /// new directory shows as `当前`.
    async fn handle_dir_card_action(
        self: &Arc<Self>,
        core: &Arc<SharedCore>,
        value: &serde_json::Value,
    ) -> Option<CardActionResult> {
        let op = value.get("op").and_then(|v| v.as_str()).unwrap_or("");
        let thread_key = thread_key_from_value(value);
        let directory = value
            .get("directory")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if directory.is_empty() {
            return Some(CardActionResult {
                card: None,
                toast: Some("无效操作".to_string()),
            });
        }
        match op {
            "pick" => {
                let current_dir = core
                    .sessions
                    .lock()
                    .await
                    .get_active(&thread_key)
                    .map(|e| e.directory.clone());
                if current_dir.as_deref() == Some(directory.as_str()) {
                    return Some(CardActionResult {
                        card: Some(self.build_dir_card_for(core, &thread_key).await),
                        toast: Some("已在当前目录".to_string()),
                    });
                }
                let session = match core
                    .opencode
                    .create_session(&core.opencode.new_session_input(Some(&directory)))
                    .await
                {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!("dir card pick session failed: {}", e);
                        return Some(CardActionResult {
                            card: None,
                            toast: Some(format!("创建会话失败：{e}")),
                        });
                    }
                };
                let entry = crate::config::SessionEntry::new(
                    thread_key.clone(),
                    session.id.clone(),
                    directory.clone(),
                );
                if let Err(e) = core.activate_session(entry).await {
                    tracing::warn!("dir card pick: persist failed: {}", e);
                }
                Some(CardActionResult {
                    card: Some(self.build_dir_card_for(core, &thread_key).await),
                    toast: Some(format!("已切换目录并新建会话（`{directory}`）")),
                })
            }
            "topic" => {
                // A topic can only be created from a non-topic message
                // (ADR-0006, ADR-0025): a `/dir` card opened inside a
                // never-bound topic — which may legally bind its single
                // session via `pick` — must not nest another topic. The guard
                // is location-based, so it also covers a topic that bound
                // itself via the row's left button on this same card.
                if let Some(rejection) = reject_nested_topic(&thread_key) {
                    return Some(rejection);
                }
                // The topic anchors on the card's own message (`open_message_id`,
                // threaded by `extract_card_action_value`), so fallback cards
                // can reply inside it — same contract as the switch card's
                // topic op (ADR-0016).
                let Some(open_message_id) = value.get("open_message_id").and_then(|v| v.as_str()) else {
                    return Some(CardActionResult {
                        card: None,
                        toast: Some("无法创建话题（缺少卡片消息引用）".to_string()),
                    });
                };
                let session = match core
                    .opencode
                    .create_session(&core.opencode.new_session_input(Some(&directory)))
                    .await
                {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!("dir card topic: create session failed: {}", e);
                        return Some(CardActionResult {
                            card: None,
                            toast: Some(format!("创建会话失败：{e}")),
                        });
                    }
                };
                // Creation title policy (ADR-0007): unnamed, like `/topic
                // <dir>`; the cover shows the directory basename until the
                // server auto-generates a title after the first exchange.
                let display = crate::bridge::command::dir_basename(&directory);
                match crate::bridge::command::open_topic_for_session(
                    core,
                    &thread_key.chat_id,
                    open_message_id,
                    &session.id,
                    directory.clone(),
                    display.clone(),
                    None,
                    None,
                )
                .await
                {
                    Ok(Some(_thread_id)) => Some(CardActionResult {
                        card: Some(self.build_dir_card_for(core, &thread_key).await),
                        toast: Some(format!("已建话题并新建会话（{display}）")),
                    }),
                    Ok(None) => Some(CardActionResult {
                        card: None,
                        toast: Some(
                            "当前会话不支持创建话题（未返回 thread_id）。请改用 `/dir <目录>` 或在飞书里手动创建话题。"
                                .to_string(),
                        ),
                    }),
                    Err(e) => {
                        tracing::warn!("dir card topic: create topic failed: {}", e);
                        Some(CardActionResult {
                            card: None,
                            toast: Some("创建话题失败".to_string()),
                        })
                    }
                }
            }
            _ => Some(CardActionResult {
                card: None,
                toast: Some("无效操作".to_string()),
            }),
        }
    }

    /// Handle an `/agent` picker-card button: record the per-session override
    /// (action `agent`), or clear it via the dedicated `agent_clear` button.
    /// Clearing is carried by the ACTION tag, never a value word — an agent
    /// literally named like a clear verb stays selectable (ADR-0020), so
    /// `clear` is a dispatch flag, not a value predicate. Refreshes the card so
    /// the current agent updates.
    async fn handle_agent_card_action(
        self: &Arc<Self>,
        core: &Arc<SharedCore>,
        value: &serde_json::Value,
        clear: bool,
    ) -> Option<CardActionResult> {
        let picked = value
            .get("value")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let thread_key = thread_key_from_value(value);
        let Some(entry) = core.sessions.lock().await.get_active(&thread_key).cloned() else {
            return Some(CardActionResult {
                card: None,
                toast: Some(format!(
                    "{}还没有会话",
                    crate::bridge::command::feishu_side_label(&thread_key)
                )),
            });
        };
        let agent = if clear { None } else { Some(picked.clone()) };
        if let Err(e) = core.update_session(&entry.session_id, |e| e.agent = agent).await {
            tracing::warn!("agent card: persist failed: {}", e);
        }
        let (card, _error) = crate::bridge::command::agent_card(core, &thread_key).await;
        Some(CardActionResult {
            card,
            toast: Some(if clear {
                "已清除 Agent（回到服务器默认）".to_string()
            } else {
                format!("Agent: {picked}（下一条消息开始生效）")
            }),
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
                let models: Vec<crate::opencode::client::ModelOption> = providers
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
                toast: Some(format!(
                    "{}还没有会话",
                    crate::bridge::command::feishu_side_label(&thread_key)
                )),
            });
        };
        entry.model = Some(picked.clone());
        // Auto-clear the `/think` variant when the new model doesn't declare
        // it (ADR-0020), same as the `/model` text form.
        let cleared_variant = core.clear_variant_for_model(&mut entry, &picked).await;
        if let Err(e) = core
            .update_session(&entry.session_id, |e| {
                e.model = entry.model.clone();
                e.variant = entry.variant.clone();
            })
            .await
        {
            tracing::warn!("model card: persist failed: {}", e);
        }
        let extra = cleared_variant
            .map(|v| format!("（已清除思考等级 `{v}`：新模型不支持）"))
            .unwrap_or_default();
        Some(CardActionResult {
            card: None,
            toast: Some(format!("Model: {picked}（下一条消息开始生效）{}", extra)),
        })
    }

    /// Handle a `/think` card button: record the chosen variant (action
    /// `think`), or clear it via the dedicated `think_clear` button. Clearing
    /// is carried by the ACTION tag, never a value word — a variant literally
    /// named like a clear verb stays selectable (ADR-0020), so `clear` is a
    /// dispatch flag, not a value predicate. Refreshes the card so the current
    /// selection updates. Best-effort validation: a pick is rejected only when
    /// the effective model is resolvable AND positively lacks the variant
    /// (ADR-0020); unknown models fall through to the server's
    /// `VariantUnavailableError`.
    async fn handle_think_card_action(
        self: &Arc<Self>,
        core: &Arc<SharedCore>,
        value: &serde_json::Value,
        clear: bool,
    ) -> Option<CardActionResult> {
        let picked = value
            .get("value")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let thread_key = thread_key_from_value(value);
        let Some(entry) = core.sessions.lock().await.get_active(&thread_key).cloned() else {
            return Some(CardActionResult {
                card: None,
                toast: Some(format!(
                    "{}还没有会话",
                    crate::bridge::command::feishu_side_label(&thread_key)
                )),
            });
        };
        if !clear
            && let Some((provider, model)) = core.effective_model(&entry.session_id).await
            && let Some(variants) = core.model_variants(&provider, &model).await
            && !variants.iter().any(|v| v == &picked)
        {
            return Some(CardActionResult {
                card: None,
                toast: Some(format!("当前模型 `{provider}/{model}` 不支持思考等级 `{picked}`")),
            });
        }
        let variant = if clear { None } else { Some(picked.clone()) };
        if let Err(e) = core
            .update_session(&entry.session_id, |e| e.variant = variant)
            .await
        {
            tracing::warn!("think card: persist failed: {}", e);
        }
        let (card, _error) = crate::bridge::command::think_card(core, &thread_key).await;
        Some(CardActionResult {
            card,
            toast: Some(if clear {
                "已清除思考等级（回到模型默认）".to_string()
            } else {
                format!("Thinking: {}（下一条消息开始生效）", picked)
            }),
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
        if let Some(e) = current_entry
            && let Err(err) = core
                .update_session(&e.session_id, |entry| entry.auto_accept = on)
                .await
        {
            tracing::warn!("autoaccept card: persist failed: {}", err);
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
                    c.acc.cola_message_id.clone(),
                )
            })
        };
        let card_id = {
            let cards = self.cards.lock().await;
            cards.get(&sid).and_then(|c| c.card_message_id.clone())
        };
        let thread_key = self.sessions.lock().await.thread_for_session(&sid);
        if !inflight
            && let Some((text, reply_to, subtitle, requester, is_group, cola_message_id)) = ctx
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
                        // Reuse the failed attempt's id so the server
                        // deduplicates — the retry is the same logical user
                        // message (ADR-0026).
                        cola_message_id,
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
