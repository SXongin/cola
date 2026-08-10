use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::bridge::command::{self, Command};
use crate::bridge::pollers::{permission_poll_loop, question_poll_loop};
use crate::bridge::render::{flush_card, render_new_turn_parts, render_parts, render_poll_loop};
use crate::bridge::session::SessionStore;
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

pub struct App {
    pub sessions: Arc<Mutex<SessionStore>>,
    pub accumulators: Arc<Mutex<HashMap<String, StreamAccumulator>>>,
    pub card_message_ids: Arc<Mutex<HashMap<String, String>>>,
    pub permission_requests: Arc<Mutex<HashMap<String, String>>>,
    /// request_id → pending question request (the AI asks the user; cola posts
    /// the answer back from the question card).
    pub question_requests: Arc<Mutex<HashMap<String, opencode::client::QuestionRequest>>>,
    pub seen_event_ids: Arc<Mutex<HashSet<String>>>,
    /// Session ids with a prompt currently in flight (serializes prompts per
    /// session so concurrent messages don't clobber each other's accumulators).
    pub inflight: Arc<Mutex<HashSet<String>>>,
    /// Default directory for new sessions (from `[bridge] work_dir`).
    pub work_dir: Option<String>,
    /// cola's own Feishu open_id, used to recognise @mentions of the bot.
    pub bot_open_id: Arc<Mutex<Option<String>>>,
    pub opencode: Arc<dyn opencode::Backend>,
    pub feishu: Arc<dyn feishu::Platform>,
}

impl App {
    pub fn new(
        cfg: Config,
        opencode: Arc<dyn opencode::Backend>,
        feishu: Arc<dyn feishu::Platform>,
    ) -> anyhow::Result<Self> {
        let session_store = SessionStore::new(cfg.bridge.session_file)?;
        Ok(Self {
            sessions: Arc::new(Mutex::new(session_store)),
            accumulators: Arc::new(Mutex::new(HashMap::new())),
            card_message_ids: Arc::new(Mutex::new(HashMap::new())),
            permission_requests: Arc::new(Mutex::new(HashMap::new())),
            question_requests: Arc::new(Mutex::new(HashMap::new())),
            seen_event_ids: Arc::new(Mutex::new(HashSet::new())),
            inflight: Arc::new(Mutex::new(HashSet::new())),
            work_dir: cfg
                .bridge
                .work_dir
                .clone()
                .map(|p| p.to_string_lossy().to_string()),
            bot_open_id: Arc::new(Mutex::new(None)),
            opencode,
            feishu,
        })
    }

    /// The directory a brand-new session starts in: `[bridge] work_dir` when
    /// configured, else the process working directory. `/dir` still overrides
    /// per session.
    fn default_session_directory(&self) -> String {
        self.work_dir
            .clone()
            .filter(|d| !d.is_empty())
            .unwrap_or_else(|| {
                std::env::current_dir()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string()
            })
    }

    pub async fn run(self: Arc<Self>) -> anyhow::Result<()> {
        // Discover cola's own open_id so @mentions of the bot can be recognised
        // and stripped from prompt text (Feishu delivers them as `@_user_N`).
        match self.feishu.bot_open_id().await {
            Ok(id) => {
                tracing::info!("bot open_id: {}", id);
                *self.bot_open_id.lock().await = Some(id);
            }
            Err(e) => tracing::warn!("failed to fetch bot open_id, mention handling disabled: {}", e),
        }

        let ws = Arc::clone(&self);
        let perm = Arc::clone(&self);
        let question = Arc::clone(&self);
        let ws_task = tokio::spawn(async move {
            if let Err(e) = feishu::ws::event_loop(&ws).await {
                tracing::error!("WS: {}", e);
            }
        });
        // Permissions are not delivered on the global SSE (typed PubSub only),
        // and a prompt can be blocked on an unanswered permission forever, so the
        // poller must run independently of any single prompt lifecycle.
        let perm_task = tokio::spawn(async move {
            if let Err(e) = permission_poll_loop(&perm).await {
                tracing::error!("Permission poller: {}", e);
            }
        });
        // Questions (the interactive `question` tool) work the same way: the AI
        // blocks until answered, the event never reaches the global SSE, so poll
        // and surface them as Feishu cards.
        let question_task = tokio::spawn(async move {
            if let Err(e) = question_poll_loop(&question).await {
                tracing::error!("Question poller: {}", e);
            }
        });
        tokio::try_join!(ws_task, perm_task, question_task)?;
        Ok(())
    }

    pub async fn handle_message(
        self: &Arc<Self>,
        message_id: String,
        chat_id: String,
        chat_type: String,
        thread_id: Option<String>,
        text: String,
    ) {
        let kind = ConversationKind::classify(&chat_type, thread_id.as_deref());
        let thread_key = kind.thread_key(&chat_id, thread_id.as_deref());
        if let Some(cmd) = command::parse_command(&text) {
            if let Err(e) = self.handle_command(cmd, thread_key, &message_id, kind).await {
                tracing::error!("Cmd: {}", e);
            }
            return;
        }
        if let Err(e) = self.handle_prompt(thread_key, text, &message_id, kind).await {
            tracing::error!("Prompt: {}", e);
        }
    }

    async fn handle_command(
        self: &Arc<Self>,
        cmd: Command,
        thread_key: ThreadKey,
        message_id: &str,
        kind: ConversationKind,
    ) -> crate::error::Result<()> {
        match cmd {
            Command::Dir(path) => {
                let session = self
                    .opencode
                    .create_session(&self.opencode.new_session_input(Some(&path)))
                    .await?;
                let entry = SessionEntry {
                    thread_key: thread_key.clone(),
                    session_id: session.id.clone(),
                    name: path.clone(),
                    directory: path.clone(),
                    agent: None,
                };
                let mut store = self.sessions.lock().await;
                store.set_active(entry);
                store.persist()?;
                self.feishu
                    .reply_text(message_id, &format!("Session moved to {}", path))
                    .await?;
            }
            Command::Switch(name) => {
                let mut store = self.sessions.lock().await;
                if store.switch(&thread_key, &name).is_some() {
                    self.feishu
                        .reply_text(message_id, &format!("Switched to \"{}\".", name))
                        .await?;
                } else {
                    self.feishu
                        .reply_text(message_id, &format!("No session matching \"{}\".", name))
                        .await?;
                }
            }
            Command::List => {
                let store = self.sessions.lock().await;
                let entries = store.list_thread(&thread_key);
                if entries.is_empty() {
                    self.feishu.reply_text(message_id, "No sessions.").await?;
                    return Ok(());
                }
                let active_id = store.get_active(&thread_key).map(|e| &e.session_id);
                let mut list = String::from("**Sessions:**\n");
                for e in &entries {
                    let mark = if active_id.map_or(false, |id| id == &e.session_id) {
                        " (active)"
                    } else {
                        ""
                    };
                    list.push_str(&format!(
                        "- {} [{}]{mark}\n  dir: {}\n",
                        e.name,
                        &e.session_id[..e.session_id.len().min(12)],
                        e.directory
                    ));
                }
                self.feishu.reply_text(message_id, &list).await?;
            }
            Command::New(name) => {
                let new_name = name.unwrap_or_else(|| format!("sess-{}", uuid::Uuid::new_v4()));
                let directory = self.default_session_directory();
                let session = self
                    .opencode
                    .create_session(&self.opencode.new_session_input(Some(&directory)))
                    .await?;
                let entry = SessionEntry {
                    thread_key: thread_key.clone(),
                    session_id: session.id.clone(),
                    name: new_name.clone(),
                    directory,
                    agent: None,
                };
                let mut store = self.sessions.lock().await;
                store.set_active(entry);
                store.persist()?;
                self.feishu
                    .reply_text(message_id, &format!("Created \"{}\".", new_name))
                    .await?;
            }
            Command::Name(name) => {
                let mut store = self.sessions.lock().await;
                if let Some(entry) = store.get_active(&thread_key) {
                    let mut e = entry.clone();
                    e.name = name.clone();
                    store.set_active(e);
                    store.persist()?;
                }
                self.feishu
                    .reply_text(message_id, &format!("Renamed to \"{}\".", name))
                    .await?;
            }
            Command::Stop => {
                if let Some(id) = self.get_session_id(&thread_key).await {
                    self.opencode.interrupt(&id).await?;
                    self.feishu.reply_text(message_id, "Interrupted.").await?;
                }
            }
            Command::Compact => {
                if let Some(id) = self.get_session_id(&thread_key).await {
                    self.opencode.compact(&id).await?;
                    self.feishu.reply_text(message_id, "Compacting...").await?;
                }
            }
            Command::Agent(name) => {
                if let Some(id) = self.get_session_id(&thread_key).await {
                    self.opencode.switch_agent(&id, &name).await?;
                    self.feishu
                        .reply_text(message_id, &format!("Agent: {}", name))
                        .await?;
                }
            }
            Command::Model(name) => {
                if let Some(id) = self.get_session_id(&thread_key).await {
                    self.opencode.switch_model(&id, &name).await?;
                    self.feishu
                        .reply_text(message_id, &format!("Model: {}", name))
                        .await?;
                }
            }
            Command::Help => {
                self.feishu.reply_text(message_id, &command::help_text()).await?;
            }
            Command::Forward(text) => {
                self.handle_prompt(thread_key, text, message_id, kind).await?;
            }
        }
        Ok(())
    }

    async fn handle_prompt(
        self: &Arc<Self>,
        thread_key: ThreadKey,
        text: String,
        message_id: &str,
        kind: ConversationKind,
    ) -> crate::error::Result<()> {
        let (mut session_id, created) = self.get_or_create_session(&thread_key, &text).await?;

        // First message on a group's top level created a lobby session: reply
        // once with guidance so the user knows each topic isolates a session.
        if created && kind == ConversationKind::GroupLobby {
            self.feishu.reply_text(message_id, GROUP_LOBBY_GUIDANCE).await?;
        }

        // Serialize prompts per session: if one is already in flight, don't let
        // a second message overwrite its accumulator (the two would race on the
        // same card). Reply with a notice instead.
        {
            let mut inflight = self.inflight.lock().await;
            if inflight.contains(&session_id) {
                drop(inflight);
                let _ = self
                    .feishu
                    .reply_text(message_id, "⏳ 上一条消息还在处理中，请稍等它完成后重发。")
                    .await;
                return Ok(());
            }
            inflight.insert(session_id.clone());
        }

        let loading = crate::feishu::card::CardBuilder::new(&text.chars().take(50).collect::<String>())
            .with_state(crate::feishu::card::CardState::Loading)
            .build();
        let card_msg_id = self.feishu.reply_card(message_id, &loading).await?;
        {
            let mut ids = self.card_message_ids.lock().await;
            ids.insert(session_id.clone(), card_msg_id);
        }
        let epoch_ms = chrono::Utc::now().timestamp_millis();
        {
            // Fresh accumulator per prompt: reuse leaks stale text/tools from the
            // previous turn into the next card.
            let mut acc = StreamAccumulator::new(&text.chars().take(50).collect::<String>());
            acc.question = text.chars().take(120).collect::<String>();
            acc.reply_to_message_id = Some(message_id.to_string());
            acc.submit_epoch_ms = Some(epoch_ms);
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
        flush_card(self, &session_id).await;

        {
            let mut inflight = self.inflight.lock().await;
            inflight.remove(&session_id);
        }

        // Permissions are handled by the independent poller spawned in App::run,
        // so a prompt blocked on a permission still gets its card shown.
        Ok(())
    }

    async fn get_session_id(&self, thread_key: &ThreadKey) -> Option<String> {
        self.sessions
            .lock()
            .await
            .get_active(thread_key)
            .map(|e| e.session_id.clone())
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
        text: &str,
        directory: String,
    ) -> crate::error::Result<String> {
        let session = self
            .opencode
            .create_session(&self.opencode.new_session_input(Some(&directory)))
            .await?;
        let entry = SessionEntry {
            thread_key: thread_key.clone(),
            session_id: session.id.clone(),
            name: text.chars().take(50).collect(),
            directory,
            agent: None,
        };
        let mut store = self.sessions.lock().await;
        store.set_active(entry);
        store.persist()?;
        Ok(session.id)
    }

    /// Handle a card action (permission Allow/Deny). Returns an updated card
    /// showing the decision, so the caller can send it back in the ack.
    pub async fn handle_card_action(&self, value: serde_json::Value) -> Option<serde_json::Value> {
        let action = value.get("action").and_then(|v| v.as_str()).unwrap_or("");
        let session_id = value.get("session_id").and_then(|v| v.as_str()).unwrap_or("");
        // Carried from the permission card for the result display
        let perm_label = value.get("perm_label").and_then(|v| v.as_str()).unwrap_or("");
        let perm_color = value
            .get("perm_color")
            .and_then(|v| v.as_str())
            .unwrap_or("green");
        let perm_body = value.get("perm_body").and_then(|v| v.as_str()).unwrap_or("");

        let mut result_card = None;
        match action {
            "perm" => {
                let reply = value.get("reply").and_then(|v| v.as_str()).unwrap_or("reject");
                let request_id = value.get("request_id").and_then(|v| v.as_str());
                if let Some(req_id) = request_id {
                    // Route the reply to the instance owning the session.
                    let directory = {
                        let store = self.sessions.lock().await;
                        store.directory_for_session(&session_id)
                    };
                    if let Err(e) = self
                        .opencode
                        .reply_permission(req_id, reply, directory.as_deref())
                        .await
                    {
                        tracing::error!("perm reply failed: {}", e);
                    } else {
                        tracing::info!("Permission reply sent: {} session={}", reply, session_id);
                        // Build the result card: shows the decision, no buttons
                        let label = if !perm_label.is_empty() { perm_label } else { reply };
                        result_card = Some(serde_json::json!({
                            "config": { "wide_screen_mode": true },
                            "header": { "title": { "tag": "plain_text", "content": label }, "template": perm_color },
                            "elements": [
                                { "tag": "markdown", "content": if perm_body.is_empty() { format!("Permission: {}", reply) } else { perm_body.to_string() } }
                            ]
                        }));
                    }
                }
            }
            "question" => {
                let reply = value.get("reply").and_then(|v| v.as_str()).unwrap_or("reject");
                let request_id = value.get("request_id").and_then(|v| v.as_str());
                if let Some(req_id) = request_id {
                    let directory = {
                        let store = self.sessions.lock().await;
                        store.directory_for_session(&session_id)
                    };
                    match reply {
                        "answer" => {
                            let index =
                                value.get("question_index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                            let answer = value
                                .get("answer")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            // Build answers in order: the chosen option for the
                            // clicked question, empty for the rest.
                            let count = {
                                let reqs = self.question_requests.lock().await;
                                reqs.get(req_id).map(|r| r.questions.len()).unwrap_or(index + 1)
                            };
                            let mut answers: Vec<Vec<String>> = Vec::new();
                            for i in 0..count {
                                answers.push(if i == index {
                                    vec![answer.clone()]
                                } else {
                                    Vec::new()
                                });
                            }
                            if let Err(e) = self
                                .opencode
                                .reply_question(req_id, &answers, directory.as_deref())
                                .await
                            {
                                tracing::error!("question reply failed: {}", e);
                            } else {
                                tracing::info!(
                                    "Question answered: {} session={} -> {:?}",
                                    req_id,
                                    session_id,
                                    answers
                                );
                                self.question_requests.lock().await.remove(req_id);
                                result_card = Some(serde_json::json!({
                                    "config": { "wide_screen_mode": true },
                                    "header": { "title": { "tag": "plain_text", "content": format!("✅ 已回答：{}", answer) }, "template": "green" },
                                    "elements": [ { "tag": "markdown", "content": format!("AI 的问题是：\n{}", answer) } ]
                                }));
                            }
                        }
                        "reject" => {
                            if let Err(e) = self.opencode.reject_question(req_id, directory.as_deref()).await
                            {
                                tracing::error!("question reject failed: {}", e);
                            } else {
                                tracing::info!("Question rejected: {}", req_id);
                                self.question_requests.lock().await.remove(req_id);
                                result_card = Some(serde_json::json!({
                                    "config": { "wide_screen_mode": true },
                                    "header": { "title": { "tag": "plain_text", "content": "🚫 已拒绝回答" }, "template": "red" },
                                    "elements": [ { "tag": "markdown", "content": "已拒绝回答 AI 的问题。" } ]
                                }));
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        result_card
    }
}

// ===== Test double support (mock Backend + recording Platform) =====

#[cfg(test)]
mod test_support {
    use super::*;

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
    }

    /// Records every card cola would send, instead of posting to Feishu.
    pub struct RecordingPlatform {
        pub calls: Arc<tokio::sync::Mutex<Vec<PlatformCall>>>,
    }

    impl RecordingPlatform {
        pub fn new() -> Self {
            Self {
                calls: Arc::new(tokio::sync::Mutex::new(Vec::new())),
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

        async fn bot_open_id(&self) -> crate::error::Result<String> {
            Ok("ou_test_bot".into())
        }
    }

    /// Serves scripted parts/permissions instead of a live OpenCode server.
    pub struct MockBackend {
        pub parts: serde_json::Value,
        pub permissions: Vec<opencode::client::PermissionRequest>,
        /// Pending questions served by `list_questions`.
        pub questions: Vec<opencode::client::QuestionRequest>,
        /// Records `reply_question` calls: (request_id, answers).
        pub reply_question_calls: Arc<tokio::sync::Mutex<Vec<(String, Vec<Vec<String>>)>>>,
        /// When set, `prompt` fails with this message (simulates a provider 503).
        pub prompt_error: Option<String>,
        /// The session id `create_session` returns.
        pub session_id: String,
        /// When true, `prompt` 404s for any session id other than `session_id`
        /// (simulates a stale mapping to a session that no longer exists).
        pub stale_session_404: bool,
    }

    impl MockBackend {
        pub fn new(parts: serde_json::Value) -> Self {
            Self {
                parts,
                permissions: Vec::new(),
                questions: Vec::new(),
                reply_question_calls: Arc::new(tokio::sync::Mutex::new(Vec::new())),
                prompt_error: None,
                session_id: "ses_test".into(),
                stale_session_404: false,
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

        async fn prompt(
            &self,
            session_id: &str,
            _text: &str,
        ) -> crate::error::Result<opencode::client::PromptResponse> {
            if self.stale_session_404 && session_id != self.session_id {
                return Err(crate::error::BridgeError::SessionNotFound(session_id.to_string()));
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

        async fn messages(
            &self,
            _session_id: &str,
        ) -> crate::error::Result<Vec<opencode::client::SessionMessage>> {
            let now = chrono::Utc::now().timestamp_millis();
            Ok(vec![opencode::client::SessionMessage {
                info: opencode::client::MessageInfo {
                    id: "msg_assist".into(),
                    role: Some("assistant".into()),
                    parent_id: Some("msg_user".into()),
                    time: Some(opencode::client::MessageTime { created: now + 1000 }),
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

        async fn reply_permission(
            &self,
            _r: &str,
            _reply: &str,
            _d: Option<&str>,
        ) -> crate::error::Result<()> {
            Ok(())
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
}

#[cfg(test)]
mod integration_tests {
    use super::test_support::*;
    use super::*;

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

        app.handle_message("msg_1".into(), "chat_1".into(), "p2p".into(), None, "hi".into())
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
        let (app, platform) = build_app(cfg, backend).await;

        // Seed a session + accumulator so the poller has a reply target.
        app.handle_message("msg_1".into(), "chat_1".into(), "p2p".into(), None, "hi".into())
            .await;

        // Run the permission poller briefly.
        tokio::spawn({
            let app = app.clone();
            async move {
                let _ = permission_poll_loop(&app).await;
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(3500)).await;

        let calls = platform.calls.lock().await.clone();
        let perm_card = calls.iter().find_map(|c| match c {
            PlatformCall::ReplyCard { card, .. } if card.to_string().contains("Permission Required") => {
                Some(card.clone())
            }
            _ => None,
        });
        let perm_card = perm_card.expect("permission card should have been sent");
        assert!(perm_card.to_string().contains("ls -la"));

        // Simulate the user clicking "Allow Once" and assert the reply routes.
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
        assert!(result.unwrap().to_string().contains("Allowed once"));
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
        let mut cfg = crate::config::load("cola.toml").expect("load cola.toml");
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
                let _ = question_poll_loop(&app).await;
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
        app.handle_message("msg_1".into(), "chat_1".into(), "p2p".into(), None, "hi".into())
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
        )
        .await;
        app.handle_message(
            "msg_2".into(),
            "oc_group_1".into(),
            "group".into(),
            None,
            "again".into(),
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

        app.handle_message("msg_1".into(), "oc_p2p_1".into(), "p2p".into(), None, "hi".into())
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
                name: "lobby".into(),
                directory: "/tmp/lobby".into(),
                agent: None,
            });
            store.set_active(crate::config::SessionEntry {
                thread_key: crate::config::ThreadKey::new("oc_group_1".into(), "omt_topic_1".into()),
                session_id: "ses_topic".into(),
                name: "topic".into(),
                directory: "/tmp/topic".into(),
                agent: None,
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
        )
        .await;
        app.handle_message(
            "msg_2".into(),
            "oc_group_1".into(),
            "group".into(),
            Some("omt_topic_1".into()),
            "refactor".into(),
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
                name: "top".into(),
                directory: "/tmp/top".into(),
                agent: None,
            });
            store.set_active(crate::config::SessionEntry {
                thread_key: crate::config::ThreadKey::new("oc_p2p_1".into(), "omt_p2p_1".into()),
                session_id: "ses_p2p_topic".into(),
                name: "ptopic".into(),
                directory: "/tmp/ptopic".into(),
                agent: None,
            });
            store.persist().unwrap();
        }

        app.handle_message("msg_1".into(), "oc_p2p_1".into(), "p2p".into(), None, "hi".into())
            .await;
        app.handle_message(
            "msg_2".into(),
            "oc_p2p_1".into(),
            "p2p".into(),
            Some("omt_p2p_1".into()),
            "topic hi".into(),
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
                name: "old2".into(),
                directory: "/tmp/old2".into(),
                agent: None,
            });
            store.set_active(crate::config::SessionEntry {
                thread_key: thread.clone(),
                session_id: "ses_old".into(),
                name: "old".into(),
                directory: "/tmp/old".into(),
                agent: None,
            });
            store.persist().unwrap();
        }

        app.handle_message("msg_1".into(), "chat_1".into(), "p2p".into(), None, "hi".into())
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
        app.question_requests.lock().await.insert(
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
                name: "x".into(),
                directory: "/work".into(),
                agent: None,
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
        assert!(result.unwrap().to_string().contains("已回答"));

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
        assert!(result.unwrap().to_string().contains("拒绝"));

        let calls = backend.reply_question_calls.lock().await.clone();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "que_1");
        assert!(calls[0].1[0][0].contains("reject"));
    }
}
