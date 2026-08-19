use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::bridge::command::{self, Command};
use crate::bridge::pollers::{external_message_poll_loop, permission_poll_loop, question_poll_loop};
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

/// Re-exec cola itself with the ORIGINAL startup args, inheriting stdio so a
/// shell log redirect (`cola ... > test.log 2>&1`) carries into the new process.
/// The current process then calls `std::process::exit(0)` right after.
fn restart_process() -> std::io::Result<()> {
    use std::process::{Command, Stdio};
    let mut exe = std::env::current_exe()?;
    // If the binary was replaced by a rebuild while we're running,
    // `/proc/self/exe` resolves to "<path> (deleted)" — spawning that fails with
    // ENOENT. Strip the suffix: the NEW binary lives at that path, which is
    // exactly what a restart wants.
    if let Some(s) = exe.to_str()
        && let Some(clean) = s.strip_suffix(" (deleted)")
    {
        exe = std::path::PathBuf::from(clean);
    }
    // `Command::new` resolves bare names via PATH; always use an absolute path.
    if !exe.is_absolute() {
        exe = std::env::current_dir()?.join(exe);
    }
    // argv[0] is the exe path; pass the rest through unchanged.
    Command::new(&exe)
        .args(std::env::args().skip(1))
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()?;
    Ok(())
}

/// The file the restart flow uses to tell the new process which chat to
/// announce the restart in.
fn restart_notify_path() -> std::path::PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".cola")
        .join("restart-notify.json")
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
}

pub struct App {
    pub sessions: Arc<Mutex<SessionStore>>,
    pub accumulators: Arc<Mutex<HashMap<String, StreamAccumulator>>>,
    pub card_message_ids: Arc<Mutex<HashMap<String, String>>>,
    /// request_id → pending question request (the AI asks the user; cola posts
    /// the answer back from the question card).
    pub question_requests: Arc<Mutex<HashMap<String, opencode::client::QuestionRequest>>>,
    /// request_id → answers recorded so far (None = not answered yet). A request
    /// is only submitted once EVERY question has an answer (or the user clicks
    /// "submit/skip"), because `reply_question` expects answers for all of them.
    pub question_partial: Arc<Mutex<QuestionPartial>>,
    /// session_id → created time of the last user message cola knows about.
    /// The external-message poller notifies Feishu when a NEWER user message
    /// appears (someone posted from OpenChamber or another shared-store client).
    pub last_user_msg_epoch: Arc<Mutex<HashMap<String, i64>>>,
    /// request_id → (card message_id, description) of the permission card cola
    /// sent. Used to mark a card stale — WITH the original request text — when
    /// the request is resolved by ANOTHER client.
    pub sent_permission_cards: Arc<Mutex<HashMap<String, (String, String)>>>,
    /// request_id → (card message_id, description) of the question card cola
    /// sent (same use).
    pub sent_question_cards: Arc<Mutex<HashMap<String, (String, String)>>>,
    pub seen_event_ids: Arc<Mutex<HashSet<String>>>,
    /// request ids already answered on the permission/question cards. Guards
    /// against double-click races (two card callbacks before the result card
    /// replaces the buttons): a second click on the same request is ignored
    /// server-side instead of re-replying.
    pub answered_requests: Arc<Mutex<HashSet<String>>>,
    /// Session ids with a prompt currently in flight (serializes prompts per
    /// session so concurrent messages don't clobber each other's accumulators).
    pub inflight: Arc<Mutex<HashSet<String>>>,
    /// Default directory for new sessions (from `[bridge] work_dir`).
    pub work_dir: Option<String>,
    /// Whether to send the group completion notice (from `[bridge] group_completion_notice`).
    pub group_completion_notice: bool,
    /// cola's own Feishu open_id, used to recognise @mentions of the bot.
    pub bot_open_id: Arc<Mutex<Option<String>>>,
    pub opencode: Arc<dyn opencode::Backend>,
    pub feishu: Arc<dyn feishu::Platform>,
}

/// Partial answers recorded for a pending question request: `answers[i]` is
/// `None` until the user answers question `i`. A request is only submitted once
/// every slot is filled (or the user clicks "submit/skip").
type QuestionPartial = HashMap<String, Vec<Option<Vec<String>>>>;

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
        let session_store = SessionStore::new(cfg.bridge.session_file)?;
        Ok(Self {
            sessions: Arc::new(Mutex::new(session_store)),
            accumulators: Arc::new(Mutex::new(HashMap::new())),
            card_message_ids: Arc::new(Mutex::new(HashMap::new())),
            question_requests: Arc::new(Mutex::new(HashMap::new())),
            question_partial: Arc::new(Mutex::new(HashMap::new())),
            last_user_msg_epoch: Arc::new(Mutex::new(HashMap::new())),
            sent_permission_cards: Arc::new(Mutex::new(HashMap::new())),
            sent_question_cards: Arc::new(Mutex::new(HashMap::new())),
            seen_event_ids: Arc::new(Mutex::new(HashSet::new())),
            answered_requests: Arc::new(Mutex::new(HashSet::new())),
            inflight: Arc::new(Mutex::new(HashSet::new())),
            work_dir: cfg
                .bridge
                .work_dir
                .clone()
                .map(|p| p.to_string_lossy().to_string()),
            group_completion_notice: cfg.bridge.group_completion_notice,
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
        // After a `/restart`, announce it in the chat that requested it.
        let notify_path = restart_notify_path();
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
        // Notify Feishu when someone posts a message from another shared-store
        // client (e.g. OpenChamber) while cola is idle on that session.
        let external = Arc::clone(&self);
        let external_task = tokio::spawn(async move {
            if let Err(e) = external_message_poll_loop(&external).await {
                tracing::error!("External message poller: {}", e);
            }
        });
        tokio::try_join!(ws_task, perm_task, question_task, external_task)?;
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
                    auto_accept: false,
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
                        crate::feishu::ws::strip_mention_tokens(&e.name),
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
                    auto_accept: false,
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
            Command::AutoAccept(switch) => {
                // With no argument (`None`) this just reports the current state;
                // `Some(on)` switches the flag AND clears requests that are
                // already pending but were seen before (the poller's `seen` set
                // skips them, so they'd otherwise hang as cards forever).
                let current = {
                    let store = self.sessions.lock().await;
                    store.get_active(&thread_key).map(|e| e.auto_accept)
                };
                let on = match switch {
                    Some(on) => on,
                    None => {
                        let state = if current.unwrap_or(false) { "开" } else { "关" };
                        self.feishu
                            .reply_text(message_id, &format!("🔁 当前会话自动审批：{}。", state))
                            .await?;
                        return Ok(());
                    }
                };
                let mut approved = 0usize;
                if let Some((sid, dir)) = {
                    let store = self.sessions.lock().await;
                    store
                        .get_active(&thread_key)
                        .map(|e| (e.session_id.clone(), e.directory.clone()))
                } {
                    {
                        let mut store = self.sessions.lock().await;
                        if let Some(entry) = store.get_active(&thread_key) {
                            let mut e = entry.clone();
                            e.auto_accept = on;
                            store.set_active(e);
                            store.persist()?;
                        }
                    }
                    if on {
                        approved = self.approve_pending_for_session(&sid, &dir).await;
                    }
                }
                let state = if on { "开" } else { "关" };
                let extra = if on && approved > 0 {
                    format!("（已自动批准 {} 条待处理请求）", approved)
                } else {
                    String::new()
                };
                self.feishu
                    .reply_text(message_id, &format!("🔁 已将会话自动审批{state}。{}", extra))
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
            Command::Help(target) => {
                let text = match target {
                    Some(name) => match command::command_help(&name) {
                        Some(h) => h,
                        None => format!("未知命令 `{}`。\n\n{}", name, command::help_text()),
                    },
                    None => command::help_text(),
                };
                self.feishu.reply_text(message_id, &text).await?;
            }
            Command::Restart => {
                // Reply BEFORE exiting, then re-exec ourselves with the SAME
                // startup args and inherited stdio (so the log redirect to
                // test.log keeps working in the new process).
                self.feishu.reply_text(message_id, "♻️ 正在重启，稍候…").await?;
                // Remember which chat to announce the restart in.
                let notify = serde_json::json!({ "chat_id": thread_key.chat_id });
                let _ = std::fs::write(restart_notify_path(), notify.to_string());
                match restart_process() {
                    Ok(()) => std::process::exit(0),
                    Err(e) => {
                        tracing::error!("restart spawn failed: {}", e);
                        self.feishu
                            .reply_text(message_id, &format!("重启失败：{}", e))
                            .await?;
                    }
                }
            }
            Command::Forward(text) => {
                self.handle_prompt(thread_key, text, message_id, kind, None, false)
                    .await?;
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
        requester_open_id: Option<String>,
        is_group: bool,
    ) -> crate::error::Result<()> {
        let (session_id, created) = self.get_or_create_session(&thread_key, &text).await?;

        // First message on a group's top level created a lobby session: reply
        // once with guidance so the user knows each topic isolates a session.
        if created && kind == ConversationKind::GroupLobby {
            self.feishu.reply_text(message_id, GROUP_LOBBY_GUIDANCE).await?;
        }

        // A `/new` (no name) session starts as `sess-<uuid>`; name it after its
        // first real prompt so the card subtitle isn't just an ID.
        {
            let mut store = self.sessions.lock().await;
            if let Some(entry) = store.get_active(&thread_key)
                && crate::feishu::card::clean_session_label(&entry.name).is_empty()
            {
                let mut e = entry.clone();
                e.name = text.chars().take(50).collect();
                store.set_active(e);
                store.persist()?;
            }
        }

        let subtitle = self.session_subtitle(&thread_key, &text).await;
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
    /// `<name> · <id-tail>` (e.g. "你好 · 01ba0ed") so both the label and the
    /// actual session are visible in p2p AND group chats. The OpenCode server's
    /// OWN session title (what OpenChamber shows) is preferred — no reliance on
    /// cola's `/new`-generated `sess-<uuid>` names. Stale names persisted before
    /// mention stripping are cleaned, and the current prompt is never echoed
    /// (the reply context already shows it).
    async fn session_subtitle(&self, thread_key: &ThreadKey, text: &str) -> String {
        let prompt_preview: String = text.chars().take(50).collect();
        let (session_id, stored_name) = {
            let store = self.sessions.lock().await;
            let Some(entry) = store.get_active(thread_key) else {
                return String::new();
            };
            (entry.session_id.clone(), entry.name.clone())
        };
        // Prefer the server's title (OpenChamber's), fall back to cola's stored
        // name (e.g. the first-prompt preview when the server has no title yet).
        let mut name = crate::feishu::card::clean_session_label(&stored_name);
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
            let mut map = self.last_user_msg_epoch.lock().await;
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

    async fn get_session_id(&self, thread_key: &ThreadKey) -> Option<String> {
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
    async fn approve_pending_for_session(&self, session_id: &str, directory: &str) -> usize {
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
            // Never persist a raw `@_user_N` mention token as the session name
            // (defense in depth — callers normally pass already-stripped text).
            name: crate::feishu::ws::strip_mention_tokens(text)
                .chars()
                .take(50)
                .collect(),
            directory,
            agent: None,
            auto_accept: false,
        };
        let mut store = self.sessions.lock().await;
        store.set_active(entry);
        store.persist()?;
        Ok(session.id)
    }

    /// Handle a card action (permission Allow/Deny, question answer/reject,
    /// error-card retry). Returns the updated card showing the decision, so the
    /// caller can send it back in the ack, plus an optional Toast for instant
    /// client feedback.
    pub async fn handle_card_action(self: &Arc<Self>, value: serde_json::Value) -> Option<CardActionResult> {
        let action = value.get("action").and_then(|v| v.as_str()).unwrap_or("");
        let session_id = value.get("session_id").and_then(|v| v.as_str()).unwrap_or("");
        // The card carries the owning directory (needed for subtask sessions
        // that aren't in the SessionStore); fall back to the SessionStore map.
        let directory = value
            .get("directory")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let directory = match directory {
            Some(d) => Some(d),
            None => self.sessions.lock().await.directory_for_session(session_id),
        };
        let directory = directory.as_deref();
        // Carried from the permission card for the result display
        let perm_label = value.get("perm_label").and_then(|v| v.as_str()).unwrap_or("");
        let perm_color = value
            .get("perm_color")
            .and_then(|v| v.as_str())
            .unwrap_or("green");
        let perm_body = value.get("perm_body").and_then(|v| v.as_str()).unwrap_or("");

        // Result cards must stay JSON 2.0: a 2.0 card cannot be "updated" back
        // to a 1.0 card in the callback response (Feishu err 200830).
        let result_card = |title: &str, template: &str, body: &str| CardActionResult {
            card: Some(serde_json::json!({
                "schema": "2.0",
                "config": { "wide_screen_mode": true },
                "header": { "title": { "tag": "plain_text", "content": title }, "template": template },
                "body": { "elements": [ { "tag": "markdown", "content": body } ] }
            })),
            toast: None,
        };

        let mut result = None;
        match action {
            "perm" => {
                let reply = value.get("reply").and_then(|v| v.as_str()).unwrap_or("reject");
                let request_id = value.get("request_id").and_then(|v| v.as_str());
                if let Some(req_id) = request_id {
                    // Inline interaction: the session has a live streaming card,
                    // so the result is NOT returned as a replacement card — the
                    // streaming card re-renders itself on the next poll.
                    let inline = self.accumulators.lock().await.contains_key(session_id);
                    // Double-click guard: once answered, a second click on the
                    // same request only re-serves the result.
                    let answered = {
                        let mut seen = self.answered_requests.lock().await;
                        if seen.contains(req_id) {
                            true
                        } else {
                            seen.insert(req_id.to_string());
                            false
                        }
                    };
                    if !answered {
                        // Route the reply to the instance owning the session.
                        if let Err(e) = self.opencode.reply_permission(req_id, reply, directory).await {
                            // The request is probably already resolved by another
                            // client (e.g. OpenChamber) — show feedback instead of
                            // leaving the user with a dead card and no response.
                            tracing::error!("perm reply failed: {}", e);
                            let mut r = result_card("⚠️ 处理失败", "red", "该权限请求可能已在其他端处理。");
                            if inline {
                                r.card = None;
                            }
                            r.toast = Some("可能已在其他端处理".to_string());
                            result = Some(r);
                            return result;
                        }
                        tracing::info!("Permission reply sent: {} session={}", reply, session_id);
                        self.sent_permission_cards.lock().await.remove(req_id);
                        if inline && let Some(acc) = self.accumulators.lock().await.get_mut(session_id) {
                            acc.pending_permissions.retain(|p| p.request_id != req_id);
                        }
                    }
                    // Result card: shows the decision, no buttons.
                    let label = if !perm_label.is_empty() { perm_label } else { reply };
                    let toast = match reply {
                        "once" => "已允许本次执行".to_string(),
                        "always" => "已允许，后续将自动放行".to_string(),
                        _ => "已拒绝".to_string(),
                    };
                    let body = if perm_body.is_empty() {
                        format!("Permission: {}", reply)
                    } else {
                        perm_body.to_string()
                    };
                    let mut r = result_card(label, perm_color, &body);
                    if inline {
                        r.card = None;
                    }
                    r.toast = Some(toast);
                    result = Some(r);
                }
            }
            "question" => {
                let reply = value.get("reply").and_then(|v| v.as_str()).unwrap_or("reject");
                let request_id = value.get("request_id").and_then(|v| v.as_str());
                if let Some(req_id) = request_id {
                    // Inline interaction: the session has a live streaming card —
                    // answered inline there, so no replacement card is returned.
                    let inline = self.accumulators.lock().await.contains_key(session_id);
                    match reply {
                        "answer" => {
                            let index =
                                value.get("question_index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                            let answer = value
                                .get("answer")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            if answer.is_empty() {
                                return None;
                            }
                            // A request is submitted ONLY when every question has
                            // an answer (`reply_question` expects all of them).
                            let already = self.answered_requests.lock().await.contains(req_id);
                            if already {
                                // finalized before (double-click): replay the result.
                                let mut r = result_card(
                                    &format!("✅ 已回答：{}", answer),
                                    "green",
                                    &format!("AI 的问题是：\n{}", answer),
                                );
                                if inline {
                                    r.card = None;
                                }
                                result = Some(r);
                            } else {
                                let n = {
                                    let reqs = self.question_requests.lock().await;
                                    reqs.get(req_id).map(|r| r.questions.len())
                                };
                                let Some(n) = n else {
                                    return None; // stale card for an already-submitted request
                                };
                                // Record this question's answer.
                                let (answered_count, slot) = {
                                    let mut partial = self.question_partial.lock().await;
                                    let slot =
                                        partial.entry(req_id.to_string()).or_insert_with(|| vec![None; n]);
                                    if index < n {
                                        slot[index] = Some(vec![answer.clone()]);
                                    }
                                    let count = slot.iter().filter(|a| a.is_some()).count();
                                    (count, slot.clone())
                                };
                                // Keep the inline card's partial answers in sync.
                                if inline
                                    && let Some(pq) = self
                                        .accumulators
                                        .lock()
                                        .await
                                        .get_mut(session_id)
                                        .and_then(|acc| {
                                            acc.pending_questions
                                                .iter_mut()
                                                .find(|pq| pq.request_id == req_id)
                                        })
                                {
                                    pq.answers = slot.clone();
                                }
                                if answered_count == n {
                                    // All questions answered → submit the whole request.
                                    self.answered_requests.lock().await.insert(req_id.to_string());
                                    let answers: Vec<Vec<String>> = self
                                        .question_partial
                                        .lock()
                                        .await
                                        .remove(req_id)
                                        .unwrap_or_default()
                                        .into_iter()
                                        .map(|a| a.unwrap_or_default())
                                        .collect();
                                    if let Err(e) =
                                        self.opencode.reply_question(req_id, &answers, directory).await
                                    {
                                        tracing::error!("question reply failed: {}", e);
                                        let mut r =
                                            result_card("⚠️ 处理失败", "red", "该问题可能已在其他端回答。");
                                        if inline {
                                            r.card = None;
                                        }
                                        r.toast = Some("可能已在其他端回答".to_string());
                                        result = Some(r);
                                        return result;
                                    }
                                    tracing::info!(
                                        "Question answered: {} session={} -> {:?}",
                                        req_id,
                                        session_id,
                                        answers
                                    );
                                    self.question_requests.lock().await.remove(req_id);
                                    self.sent_question_cards.lock().await.remove(req_id);
                                    if inline
                                        && let Some(acc) = self.accumulators.lock().await.get_mut(session_id)
                                    {
                                        acc.pending_questions.retain(|pq| pq.request_id != req_id);
                                    }
                                    let mut r = result_card(
                                        &format!("✅ 已回答：{}", answer),
                                        "green",
                                        &format!("AI 的问题是：\n{}", answer),
                                    );
                                    if inline {
                                        r.card = None;
                                    }
                                    r.toast = Some("已回答".to_string());
                                    result = Some(r);
                                } else if inline {
                                    // Inline: the streaming card re-renders with the
                                    // updated partial answers — toast only.
                                    let mut r = CardActionResult {
                                        card: None,
                                        toast: None,
                                    };
                                    r.toast = Some(format!("已记录答案，还有 {} 题未答", n - answered_count));
                                    result = Some(r);
                                } else {
                                    // Still questions left: return an updated card that
                                    // shows the answered ones as done and the rest open.
                                    let remaining = n - answered_count;
                                    let req = {
                                        let reqs = self.question_requests.lock().await;
                                        reqs.get(req_id).cloned()
                                    };
                                    let req = req?;
                                    let partial = self
                                        .question_partial
                                        .lock()
                                        .await
                                        .get(req_id)
                                        .cloned()
                                        .unwrap_or_else(|| vec![None; n]);
                                    let card = crate::feishu::card::build_question_card(
                                        req_id,
                                        &req.session_id,
                                        &req.questions,
                                        directory.unwrap_or(""),
                                        &partial,
                                    );
                                    let mut r = CardActionResult {
                                        card: Some(card),
                                        toast: None,
                                    };
                                    r.toast = Some(format!("已记录答案，还有 {} 题未答", remaining));
                                    result = Some(r);
                                }
                            }
                        }
                        "submit" => {
                            // Finalize with whatever was answered (empty for the rest).
                            let already = self.answered_requests.lock().await.contains(req_id);
                            if already {
                                let mut r = result_card("✅ 已回答", "green", "已提交 AI 的问题答案。");
                                if inline {
                                    r.card = None;
                                }
                                result = Some(r);
                            } else {
                                self.answered_requests.lock().await.insert(req_id.to_string());
                                let answers: Vec<Vec<String>> = self
                                    .question_partial
                                    .lock()
                                    .await
                                    .remove(req_id)
                                    .unwrap_or_default()
                                    .into_iter()
                                    .map(|a| a.unwrap_or_default())
                                    .collect();
                                if let Err(e) =
                                    self.opencode.reply_question(req_id, &answers, directory).await
                                {
                                    tracing::error!("question reply failed: {}", e);
                                    let mut r =
                                        result_card("⚠️ 处理失败", "red", "该问题可能已在其他端回答。");
                                    if inline {
                                        r.card = None;
                                    }
                                    r.toast = Some("可能已在其他端回答".to_string());
                                    result = Some(r);
                                    return result;
                                }
                                tracing::info!(
                                    "Question submitted: {} session={} -> {:?}",
                                    req_id,
                                    session_id,
                                    answers
                                );
                                self.question_requests.lock().await.remove(req_id);
                                if inline
                                    && let Some(acc) = self.accumulators.lock().await.get_mut(session_id)
                                {
                                    acc.pending_questions.retain(|pq| pq.request_id != req_id);
                                }
                                let labels: Vec<&str> =
                                    answers.iter().flatten().map(|s| s.as_str()).collect();
                                let summary = if labels.is_empty() {
                                    "已提交".to_string()
                                } else {
                                    labels.join("、")
                                };
                                let mut r = result_card(
                                    &format!("✅ 已回答：{}", summary),
                                    "green",
                                    &format!("AI 的问题是：\n{}", summary),
                                );
                                if inline {
                                    r.card = None;
                                }
                                r.toast = Some("已提交".to_string());
                                result = Some(r);
                            }
                        }
                        "reject" => {
                            let already = self.answered_requests.lock().await.contains(req_id);
                            if already {
                                let mut r = result_card("🚫 已拒绝回答", "red", "已拒绝回答 AI 的问题。");
                                if inline {
                                    r.card = None;
                                }
                                result = Some(r);
                            } else {
                                self.answered_requests.lock().await.insert(req_id.to_string());
                                if let Err(e) = self.opencode.reject_question(req_id, directory).await {
                                    tracing::error!("question reject failed: {}", e);
                                    let mut r =
                                        result_card("⚠️ 处理失败", "red", "该问题可能已在其他端回答。");
                                    if inline {
                                        r.card = None;
                                    }
                                    r.toast = Some("可能已在其他端回答".to_string());
                                    result = Some(r);
                                    return result;
                                }
                                tracing::info!("Question rejected: {}", req_id);
                                self.question_requests.lock().await.remove(req_id);
                                self.question_partial.lock().await.remove(req_id);
                                self.sent_question_cards.lock().await.remove(req_id);
                                if inline
                                    && let Some(acc) = self.accumulators.lock().await.get_mut(session_id)
                                {
                                    acc.pending_questions.retain(|pq| pq.request_id != req_id);
                                }
                                let mut r = result_card("🚫 已拒绝回答", "red", "已拒绝回答 AI 的问题。");
                                if inline {
                                    r.card = None;
                                }
                                r.toast = Some("已拒绝回答".to_string());
                                result = Some(r);
                            }
                        }
                        _ => {}
                    }
                }
            }
            "retry" => {
                // Re-submit the failed prompt on the SAME card. The card callback
                // must ack within 3s, so spawn the prompt pipeline and return a
                // "retrying" card immediately; run_prompt then resets the card to
                // Loading and streams the new attempt into it.
                let sid = value
                    .get("session_id")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or_default();
                if !sid.is_empty() {
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
                        let mut r = result_card("⏳ 正在重试...", "blue", "已重新提交原始问题。");
                        r.toast = Some("正在重试...".to_string());
                        result = Some(r);
                    } else {
                        // Nothing to retry (no stored prompt / card, or a prompt is
                        // already in flight): keep the card as it is.
                        tracing::warn!(
                            "retry: no retryable context for session {} (inflight={})",
                            sid,
                            inflight
                        );
                    }
                }
            }
            _ => {}
        }
        result
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
    }

    impl RecordingPlatform {
        pub fn new() -> Self {
            Self {
                calls: Arc::new(tokio::sync::Mutex::new(Vec::new())),
                user_names: std::collections::HashMap::new(),
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

        async fn bot_open_id(&self) -> crate::error::Result<String> {
            Ok("ou_test_bot".into())
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
        pub session_titles: std::collections::HashMap<String, String>,
        /// Pending questions served by `list_questions`.
        pub questions: Vec<opencode::client::QuestionRequest>,
        /// Records `reply_question` calls: (request_id, answers).
        pub reply_question_calls: Arc<tokio::sync::Mutex<Vec<(String, Vec<Vec<String>>)>>>,
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
    }

    impl MockBackend {
        pub fn new(parts: serde_json::Value) -> Self {
            Self {
                parts,
                permissions: Vec::new(),
                external_user_message: None,
                reply_permission_calls: Arc::new(tokio::sync::Mutex::new(Vec::new())),
                session_titles: std::collections::HashMap::new(),
                questions: Vec::new(),
                reply_question_calls: Arc::new(tokio::sync::Mutex::new(Vec::new())),
                prompt_error: None,
                fail_prompt_count: std::sync::atomic::AtomicUsize::new(0).into(),
                prompt_calls: Arc::new(tokio::sync::Mutex::new(Vec::new())),
                prompt_async_calls: Arc::new(tokio::sync::Mutex::new(Vec::new())),
                session_id: "ses_test".into(),
                stale_session_404: false,
                session_parents: std::collections::HashMap::new(),
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
            self.prompt_async_calls.lock().await.push(format!("{}:{}", session_id, text));
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
                title: self.session_titles.get(session_id).cloned(),
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
    async fn subtitle_shows_name_and_id_tail() {
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
                name: "你好".into(),
                directory: "/tmp/x".into(),
                agent: None,
                auto_accept: false,
            });
        }

        // A later message in the same session → name + id tail.
        assert_eq!(app.session_subtitle(&key, "另一个问题").await, "你好 · 01ba0ed");
        // First message (name == prompt preview) → no echo, but still the id.
        assert_eq!(app.session_subtitle(&key, "你好").await, "01ba0ed");
        // `/new`-generated `sess-<uuid>` names are meaningless → just the ID tail.
        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: key.clone(),
                session_id: "ses_00ea4e77cffez1fo4wrNuJyHF0".into(),
                name: "sess-7a025fa5-74a1-44e0-b5c5-80b9a21f71bc".into(),
                directory: "/tmp/y".into(),
                agent: None,
                auto_accept: false,
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
        let mut mock = MockBackend::new(realistic_parts());
        mock.session_titles
            .insert("ses_test".into(), "OpenChamber 显示的标题".into());
        let (app, _) = build_app(cfg, mock).await;
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());

        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: key.clone(),
                session_id: "ses_test".into(),
                name: "sess-7a025fa5-74a1-44e0-b5c5-80b9a21f71bc".into(),
                directory: "/tmp/x".into(),
                agent: None,
                auto_accept: false,
            });
        }
        assert_eq!(
            app.session_subtitle(&key, "问题").await,
            "OpenChamber 显示的标题 · test"
        );
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
                let _ = permission_poll_loop(&app).await;
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
                name: "aa".into(),
                directory: "/tmp/aa".into(),
                agent: None,
                auto_accept: true,
            });
            store.persist().unwrap();
        }

        tokio::spawn({
            let app = app.clone();
            async move {
                let _ = permission_poll_loop(&app).await;
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
                name: "aa".into(),
                directory: "/tmp/aa".into(),
                agent: None,
                auto_accept: false,
            });
            store.persist().unwrap();
        }

        // Now the user turns autoaccept on via the command.
        app.handle_command(
            Command::AutoAccept(Some(true)),
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
                name: "aa".into(),
                directory: "/tmp/aa".into(),
                agent: None,
                auto_accept: false,
            });
            store.persist().unwrap();
        }

        app.handle_command(
            Command::AutoAccept(Some(true)),
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
        app.sent_permission_cards
            .lock()
            .await
            .insert("per_stale".into(), ("om_sent_card".into(), "bash ls -la".into()));

        tokio::spawn({
            let app = app.clone();
            async move {
                let _ = permission_poll_loop(&app).await;
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
                name: "共享会话".into(),
                directory: "/tmp/ext".into(),
                agent: None,
                auto_accept: false,
            });
            store.persist().unwrap();
        }
        // Baseline: a minute ago, so the fresh user message is "new".
        let baseline = chrono::Utc::now().timestamp_millis() - 60_000;
        app.last_user_msg_epoch
            .lock()
            .await
            .insert("ses_ext".into(), baseline);

        tokio::spawn({
            let app = app.clone();
            async move {
                let _ = external_message_poll_loop(&app).await;
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
        let (app, platform) = build_app(cfg, backend).await;

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
                let _ = permission_poll_loop(&app).await;
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(3500)).await;

        // The permission card for the child session must reach the parent's chat
        // (no "No reply target" warning, no SendCard-only: a ReplyCard suffices).
        let calls = platform.calls.lock().await.clone();
        let perm_card = calls.iter().find_map(|c| match c {
            PlatformCall::ReplyCard { card, .. } if card.to_string().contains("git status") => {
                Some(card.clone())
            }
            _ => None,
        });
        let perm_card = perm_card.expect("subtask permission card should be sent to the parent");
        // JSON 2.0: buttons sit directly in body.elements; grab the first one.
        let first_button = perm_card["body"]["elements"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["tag"] == "button")
            .expect("permission card has buttons");
        let value = first_button["value"].clone();
        assert_eq!(value["session_id"], child);
        // The card must carry the owning directory so the reply routes to the
        // right instance even though the child session isn't in the store.
        assert!(
            value["directory"]
                .as_str()
                .map(|d| !d.is_empty())
                .unwrap_or(false),
            "permission card must carry a directory, got: {}",
            value
        );

        // Clicking Allow routes the reply with that directory.
        let mut value = value;
        value["reply"] = serde_json::json!("once");
        value["perm_label"] = serde_json::json!("✅ Allowed once");
        value["perm_color"] = serde_json::json!("green");
        let result = app.handle_card_action(value).await;
        assert!(result.is_some(), "reply should succeed for subtask session");
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
                name: "lobby".into(),
                directory: "/tmp/lobby".into(),
                agent: None,
                auto_accept: false,
            });
            store.set_active(crate::config::SessionEntry {
                thread_key: crate::config::ThreadKey::new("oc_group_1".into(), "omt_topic_1".into()),
                session_id: "ses_topic".into(),
                name: "topic".into(),
                directory: "/tmp/topic".into(),
                agent: None,
                auto_accept: false,
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
                name: "top".into(),
                directory: "/tmp/top".into(),
                agent: None,
                auto_accept: false,
            });
            store.set_active(crate::config::SessionEntry {
                thread_key: crate::config::ThreadKey::new("oc_p2p_1".into(), "omt_p2p_1".into()),
                session_id: "ses_p2p_topic".into(),
                name: "ptopic".into(),
                directory: "/tmp/ptopic".into(),
                agent: None,
                auto_accept: false,
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
                name: "old2".into(),
                directory: "/tmp/old2".into(),
                agent: None,
                auto_accept: false,
            });
            store.set_active(crate::config::SessionEntry {
                thread_key: thread.clone(),
                session_id: "ses_old".into(),
                name: "old".into(),
                directory: "/tmp/old".into(),
                agent: None,
                auto_accept: false,
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
                auto_accept: false,
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
        app.question_requests.lock().await.insert(
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
        app.question_requests.lock().await.insert(
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
                let _ = question_poll_loop(&app).await;
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
}
