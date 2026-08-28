/// Parsed slash command from user message text.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// Change project directory for the current session
    Dir(String),
    /// Switch to a session by title/directory/id, adopting foreign sessions
    Switch(String),
    /// List sessions in the shared store (`/list [keyword] [--all]`)
    List { keyword: Option<String>, all: bool },
    /// Create a fresh session, optionally named
    New(Option<String>),
    /// Adopt an arbitrary server session into this thread (`/attach <id|title> [--force]`)
    Attach { query: String, force: bool },
    /// Un-map the current thread's session (server session stays untouched)
    Forget,
    /// Create a real Feishu topic backed by a fresh session rooted at `directory`.
    Topic { directory: String, name: Option<String> },
    /// Rename the current session (PATCHes the server title)
    Name(String),
    /// Interrupt the current session execution
    Stop,
    /// Compact the current session context
    Compact,
    /// Switch agent in the current session
    Agent(String),
    /// Switch model in the current session
    Model(String),
    /// Auto-accept of permission requests for the current session.
    AutoAccept(AutoAcceptAction),
    /// Restart cola itself, preserving startup args and the log redirect.
    Restart,
    /// Restart the OpenCode server — but only when cola started it. A server
    /// another tool launched is never touched.
    RestartOpenCode,
    /// Show available commands, or help for one command (`/help <cmd>`).
    Help(Option<String>),
    /// Forward unrecognized slash command to OpenCode as prompt text
    Forward(String),
}

/// What `/autoaccept` should do: report the current state, or switch it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoAcceptAction {
    /// No argument — just report whether autoaccept is currently on.
    Status,
    /// `on` / `off` — switch the flag.
    Set(bool),
}

/// Parse a slash command from message text. Returns `None` if the message
/// is not a command (plain text).
pub fn parse_command(text: &str) -> Option<Command> {
    let trimmed = text.trim();
    if !trimmed.starts_with('/') {
        return None;
    }

    let parts: Vec<&str> = trimmed.splitn(2, ' ').collect();
    let cmd = parts[0].to_lowercase();
    let arg = parts.get(1).map(|s| s.trim()).filter(|s| !s.is_empty());

    match cmd.as_str() {
        // Commands that need an arg show their own help when the arg is missing,
        // instead of silently becoming an AI prompt.
        "/dir" => match arg {
            Some(p) => Some(Command::Dir(p.to_string())),
            None => Some(Command::Help(Some("dir".into()))),
        },
        "/switch" => match arg {
            Some(p) => Some(Command::Switch(p.to_string())),
            None => Some(Command::Help(Some("switch".into()))),
        },
        // `/list [keyword] [--all]` — keyword may be multiple words or a flag.
        "/list" => {
            let mut keyword = None;
            let mut all = false;
            if let Some(a) = arg {
                let mut words: Vec<&str> = Vec::new();
                for w in a.split_whitespace() {
                    if w == "--all" {
                        all = true;
                    } else {
                        words.push(w);
                    }
                }
                if !words.is_empty() {
                    keyword = Some(words.join(" "));
                }
            }
            Some(Command::List { keyword, all })
        }
        // `/attach <id|title> [--force]`
        "/attach" => match arg {
            Some(a) => {
                let words: Vec<&str> = a.split_whitespace().collect();
                let force = words.contains(&"--force");
                let query: String = words
                    .iter()
                    .filter(|w| **w != "--force")
                    .cloned()
                    .collect::<Vec<&str>>()
                    .join(" ");
                if query.is_empty() {
                    Some(Command::Help(Some("attach".into())))
                } else {
                    Some(Command::Attach { query, force })
                }
            }
            None => Some(Command::Help(Some("attach".into()))),
        },
        "/forget" => Some(Command::Forget),
        "/new" => Some(Command::New(arg.map(|s| s.to_string()))),
        "/topic" => match arg {
            Some(a) => {
                // `/topic <dir>` or `/topic <dir> <name>`
                let mut it = a.splitn(2, ' ');
                let dir = it.next().unwrap_or("").trim();
                let name = it.next().map(|s| s.trim()).filter(|s| !s.is_empty());
                if dir.is_empty() {
                    Some(Command::Help(Some("topic".into())))
                } else {
                    Some(Command::Topic {
                        directory: dir.to_string(),
                        name: name.map(|s| s.to_string()),
                    })
                }
            }
            None => Some(Command::Help(Some("topic".into()))),
        },
        "/name" => match arg {
            Some(p) => Some(Command::Name(p.to_string())),
            None => Some(Command::Help(Some("name".into()))),
        },
        "/stop" => Some(Command::Stop),
        "/compact" => Some(Command::Compact),
        "/agent" => match arg {
            Some(p) => Some(Command::Agent(p.to_string())),
            None => Some(Command::Help(Some("agent".into()))),
        },
        "/model" => match arg {
            Some(p) => Some(Command::Model(p.to_string())),
            None => Some(Command::Help(Some("model".into()))),
        },
        // `/autoaccept` reports the current state; `/autoaccept on|off` switches.
        "/autoaccept" => match arg {
            Some("on") | Some("true") | Some("1") => Some(Command::AutoAccept(AutoAcceptAction::Set(true))),
            Some("off") | Some("false") | Some("0") => {
                Some(Command::AutoAccept(AutoAcceptAction::Set(false)))
            }
            Some(other) => match other.parse::<bool>() {
                Ok(b) => Some(Command::AutoAccept(AutoAcceptAction::Set(b))),
                Err(_) => Some(Command::Help(Some("autoaccept".into()))),
            },
            None => Some(Command::AutoAccept(AutoAcceptAction::Status)),
        },
        "/restart" => Some(Command::Restart),
        "/restart-opencode" => Some(Command::RestartOpenCode),
        "/help" => Some(Command::Help(arg.map(|s| s.to_lowercase()))),
        // `/init`, `/review`, or any unknown /command — forward to OpenCode
        _ => Some(Command::Forward(trimmed.to_string())),
    }
}

/// Help text shown for the `/help` command.
pub fn help_text() -> String {
    "\
**cola commands**
`/dir <path>` · Open a new session in <path>
`/switch <name>` · Switch to a session (adopts foreign ones)
`/list [keyword] [--all]` · List recent sessions across the store
`/attach <id|title> [--force]` · Take over a session into this chat
`/forget` · Un-map this chat's session (server session stays)
`/new [name]` · Create a new session
`/topic <dir> [name]` · Create a new Feishu topic + session in <dir>
`/name <name>` · Rename current session (server-side)
`/stop` · Interrupt execution
`/compact` · Compact context
`/agent <name>` · Switch agent
`/model <p/m>` · Switch model
`/autoaccept` · Show auto-approve status; `/autoaccept on|off` switches
`/restart` · Restart cola (keeps startup args + log redirect)
`/restart-opencode` · Restart the OpenCode server (only when cola started it)
`/help <command>` · Show help for one command (e.g. `/help model`)

话题规则：已绑定会话的话题里，`/list`、`/switch`、`/attach`、`/new`、`/dir` 被拒绝，请回主对话操作。从未绑定过会话的话题可以用它们来绑定该话题的唯一会话。
    "
    .to_string()
}

/// Detailed help for one command (`/help <command>`). `None` for unknown names.
pub fn command_help(name: &str) -> Option<String> {
    let text = match name.to_lowercase().as_str() {
        "dir" => {
            "/dir <path>\nOpen a NEW session in <path> (create a session rooted at that directory).\nExample: `/dir /root/proj/lib`"
        }
        "switch" => {
            "/switch <keyword>\nSwitch to a session by title, directory or id. The current chat's sessions win; otherwise a unique match in the shared store is adopted. Ambiguous keywords list candidates.\nExample: `/switch backend`"
        }
        "list" => {
            "/list [keyword] [--all]\nList recently-active sessions across the shared store (up to 15): title, directory, id and last activity. A keyword filters by title/directory/id; `--all` also shows sub-task child sessions.\nExample: `/list cola`"
        }
        "attach" => {
            "/attach <id|title> [--force]\nTake over a session created outside Feishu into this chat. Resolution: exact id → unique id-prefix → unique title substring. If the session already belongs to another chat, show its owner and reject unless `--force`.\nExample: `/attach ses_abc123`"
        }
        "forget" => {
            "/forget\nUn-map this chat's session. The server session stays untouched and can be adopted again.\nExample: `/forget`"
        }
        "new" => {
            "/new [name]\nCreate a fresh session, optionally named (the name is PATCHed server-side). Without a name the server generates one after the first message.\nExample: `/new api-refactor`"
        }
        "topic" => {
            "/topic <dir> [name]\nCreate a real Feishu topic backed by a new session rooted at <dir>. The topic is UI-separated from the current conversation, so you can switch between topics in the Feishu client. Reply inside the created topic to talk to that session.\nExample: `/topic /root/proj/lib api-refactor`"
        }
        "name" => {
            "/name <name>\nRename the current session server-side (visible to every client sharing the store).\nExample: `/name frontend`"
        }
        "stop" => {
            "/stop\nInterrupt the current execution (aborts the running prompt, e.g. a stuck question or tool)."
        }
        "compact" => {
            "/compact\nCompact the current session's context: summarize older messages to free context window."
        }
        "agent" => "/agent <name>\nSwitch the agent used by the current session.\nExample: `/agent build`",
        "model" => {
            "/model <provider/model>\nSwitch the model for the current session.\nExample: `/model opencode-go/deepseek-v4-flash`"
        }
        "autoaccept" => {
            "/autoaccept [on|off]\nShow or switch auto-allowing permission requests for this session (no permission cards).\nNo arg: show current state. `/autoaccept on` / `/autoaccept off` switch it.\nExample: `/autoaccept`"
        }
        "restart" => {
            "/restart\nRestart cola itself, keeping startup args and the log redirect. The new process takes over the singleton lock (passes --replace). cola announces in this chat when it's back."
        }
        "restart-opencode" => {
            "/restart-opencode\nRestart the OpenCode server. cola only restarts a server IT started; a server launched by another tool is left alone and needs a manual restart."
        }
        "help" => {
            "/help [command]\nList all commands, or show detailed help for one.\nExample: `/help model`"
        }
        _ => return None,
    };
    Some(text.to_string())
}

// ===== Command execution (the Command flow) =====
// Moved out of handler.rs so the bridge coordinator stays thin; these methods
// run the parsed slash commands against the shared core.

use crate::bridge::handler::App;
use crate::config::{ConversationKind, SessionEntry, ThreadKey};
use std::sync::Arc;

/// Re-exec cola itself with the ORIGINAL startup args, inheriting stdio so a
/// shell log redirect (`cola ... > test.log 2>&1`) carries into the new process.
/// `--replace` is appended so the new process can take over the singleton lock
/// even while the old process lingers (briefly alive, then a zombie until the
/// launching shell reaps it). The current process then calls
/// `std::process::exit(0)` right after.
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
    // argv[0] is the exe path; pass the rest through unchanged, plus `--replace`
    // so the restarted cola is allowed to take the singleton lock from us.
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if !args.iter().any(|a| a == "--replace") {
        args.push("--replace".to_string());
    }
    Command::new(&exe)
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()?;
    Ok(())
}

/// The file the restart flow uses to tell the new process which chat to
/// announce the restart in.
pub(crate) fn restart_notify_path() -> std::path::PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".cola")
        .join("restart-notify.json")
}

impl App {
    pub(crate) async fn handle_command(
        self: &Arc<Self>,
        cmd: Command,
        thread_key: ThreadKey,
        message_id: &str,
        kind: ConversationKind,
    ) -> crate::error::Result<()> {
        // Topic single-session gate (ADR-0007): inside a topic that already has
        // a session, the session-selection/creation commands are rejected. A
        // topic that never had a session may use them — their outcome becomes
        // that topic's single session.
        if kind == ConversationKind::Topic {
            let blocked = matches!(
                cmd,
                Command::Dir(_)
                    | Command::Switch(_)
                    | Command::List { .. }
                    | Command::New(_)
                    | Command::Attach { .. }
            );
            if blocked {
                let has_session = self.sessions.lock().await.get_active(&thread_key).is_some();
                if has_session {
                    self.feishu
                        .reply_text(message_id, "⚠️ 话题已绑定会话，请回主对话操作。")
                        .await?;
                    return Ok(());
                }
            }
        }
        match cmd {
            Command::Dir(path) => {
                let session = self
                    .opencode
                    .create_session(&self.opencode.new_session_input(Some(&path)))
                    .await?;
                let entry = SessionEntry {
                    thread_key: thread_key.clone(),
                    session_id: session.id.clone(),
                    directory: path.clone(),
                    agent: None,
                    auto_accept: false,
                    topic_anchor: None,
                };
                let mut store = self.sessions.lock().await;
                store.set_active(entry);
                store.persist()?;
                self.invalidate_session_list_cache().await;
                self.feishu
                    .reply_text(message_id, &format!("Session moved to {}", path))
                    .await?;
            }
            Command::Switch(keyword) => {
                self.handle_switch(&thread_key, &keyword, message_id, kind)
                    .await?;
            }
            Command::List { keyword, all } => {
                self.handle_list(&thread_key, keyword.as_deref(), all, message_id)
                    .await?;
            }
            Command::New(name) => {
                let directory = self.default_session_directory();
                let session = self
                    .opencode
                    .create_session(&self.opencode.new_session_input(Some(&directory)))
                    .await?;
                // Creation title policy (ADR-0007): `/new <name>` PATCHes the
                // title immediately; `/new` (no name) leaves the server default
                // so a title is auto-generated after the first message.
                if let Some(n) = &name {
                    self.opencode.update_session_title(&session.id, n).await?;
                }
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
                let label = name.unwrap_or_else(|| format!("sess-{}", uuid::Uuid::new_v4()));
                self.feishu
                    .reply_text(message_id, &format!("Created \"{}\".", label))
                    .await?;
            }
            Command::Attach { query, force } => {
                self.handle_attach(&thread_key, &query, force, message_id, kind)
                    .await?;
            }
            Command::Forget => {
                let mut store = self.sessions.lock().await;
                let removed = store.remove_thread(&thread_key);
                store.persist()?;
                self.invalidate_session_list_cache().await;
                if removed.is_empty() {
                    self.feishu.reply_text(message_id, "当前没有映射的会话。").await?;
                } else {
                    self.feishu
                        .reply_text(
                            message_id,
                            "已解除本会话的映射（服务器会话仍保留，可用 `/list` 重新找到）。",
                        )
                        .await?;
                }
            }
            Command::Topic { directory, name } => {
                // Opening a topic from inside another topic would nest
                // confusingly; only create topics from a non-topic message.
                if kind == ConversationKind::Topic {
                    self.feishu
                        .reply_text(
                            message_id,
                            "⚠️ /topic 只能从会话顶层使用，不能在话题里再开话题。请在主会话里发 /topic <目录>。",
                        )
                        .await?;
                    return Ok(());
                }
                let session = self
                    .opencode
                    .create_session(&self.opencode.new_session_input(Some(&directory)))
                    .await?;
                // Creation title policy (ADR-0007): a named `/topic` PATCHes the
                // title; without a name the server default is left for
                // auto-generation. The display name only drives the anchor text.
                if let Some(n) = &name {
                    self.opencode.update_session_title(&session.id, n).await?;
                }
                let display_name = name.unwrap_or_else(|| {
                    directory
                        .trim_end_matches('/')
                        .rsplit('/')
                        .next()
                        .filter(|s| !s.is_empty())
                        .unwrap_or(&directory)
                        .to_string()
                });
                // Create a real topic anchored on the user's command message.
                // `anchor` is the created reply's own message_id — a message
                // INSIDE the topic, so permission/question/external cards that
                // must be sent (no streaming card) can reply to it and stay in
                // the topic (the create API rejects `thread_id` as a target).
                let (anchor, thread_id) = self
                    .feishu
                    .reply_in_thread(
                        message_id,
                        &format!(
                            "📌 已创建会话 `{}`（目录 `{}`）。\n请在本话题内回复，即可和这个会话对话。",
                            display_name, directory
                        ),
                    )
                    .await?;
                let Some(thread_id) = thread_id else {
                    tracing::warn!(
                        "topic: no thread_id returned for /topic in chat {}; not mapping session",
                        thread_key.chat_id
                    );
                    self.feishu
                        .reply_text(
                            message_id,
                            "⚠️ 当前会话不支持创建话题（未返回 thread_id）。请改用 `/dir <目录>` 或在飞书里手动创建话题。",
                        )
                        .await?;
                    return Ok(());
                };
                let topic_key = crate::config::ThreadKey::new(thread_key.chat_id.clone(), thread_id.clone());
                let entry = crate::config::SessionEntry {
                    thread_key: topic_key,
                    session_id: session.id.clone(),
                    directory,
                    agent: None,
                    auto_accept: false,
                    topic_anchor: Some(anchor),
                };
                let mut store = self.sessions.lock().await;
                store.set_active(entry);
                store.persist()?;
                self.invalidate_session_list_cache().await;
                tracing::info!(
                    "topic: created topic {} for session {} in chat {}",
                    thread_id,
                    session.id,
                    thread_key.chat_id
                );
            }
            Command::Name(name) => {
                // `/name` PATCHes the server title (ADR-0007): the change is
                // visible to every client sharing the store, and the `/list`
                // cache is invalidated so the new title shows immediately.
                if let Some(id) = self.get_session_id(&thread_key).await {
                    self.opencode.update_session_title(&id, &name).await?;
                    self.invalidate_session_list_cache().await;
                }
                self.feishu
                    .reply_text(message_id, &format!("Renamed to \"{}\".", name))
                    .await?;
            }
            Command::AutoAccept(action) => {
                // `Status` reports the current state; `Set(on)` switches the flag
                // AND clears requests that are already pending but were seen
                // before (the poller's `seen` set skips them, so they'd
                // otherwise hang as cards forever).
                let entry = {
                    let store = self.sessions.lock().await;
                    store.get_active(&thread_key).cloned()
                };
                match action {
                    crate::bridge::command::AutoAcceptAction::Status => {
                        let state = if entry.as_ref().map(|e| e.auto_accept).unwrap_or(false) {
                            "开"
                        } else {
                            "关"
                        };
                        self.feishu
                            .reply_text(message_id, &format!("🔁 当前会话自动审批：{}。", state))
                            .await?;
                        return Ok(());
                    }
                    crate::bridge::command::AutoAcceptAction::Set(on) => {
                        let approved = if on {
                            if let Some(e) = &entry {
                                self.approve_pending_for_session(&e.session_id, &e.directory)
                                    .await
                            } else {
                                0
                            }
                        } else {
                            0
                        };
                        if let Some(e) = entry {
                            let mut store = self.sessions.lock().await;
                            let mut e = e.clone();
                            e.auto_accept = on;
                            store.set_active(e);
                            store.persist()?;
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
                }
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
                    Some(name) => match command_help(&name) {
                        Some(h) => h,
                        None => format!("未知命令 `{}`。\n\n{}", name, help_text()),
                    },
                    None => help_text(),
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
            Command::RestartOpenCode => {
                // Restart the OpenCode server, but ONLY when cola started it.
                // A server launched by another tool is never touched: cola has
                // no record of its configuration, and killing it could take
                // down another application's runtime.
                match crate::bridge::discovery::restart_self_spawned_server().await {
                    Ok(crate::bridge::discovery::RestartOutcome::Restarted) => {
                        self.feishu
                            .reply_text(message_id, "♻️ 已重启 OpenCode 服务器。")
                            .await?;
                    }
                    Ok(crate::bridge::discovery::RestartOutcome::NotOwned) => {
                        self.feishu
                            .reply_text(
                                message_id,
                                "这个 OpenCode 服务器不是 cola 启动的，需要你手动重启它。",
                            )
                            .await?;
                    }
                    Ok(crate::bridge::discovery::RestartOutcome::NoServer) => {
                        self.feishu
                            .reply_text(message_id, "当前没有正在运行的 OpenCode 服务器。")
                            .await?;
                    }
                    Err(e) => {
                        tracing::error!("restart opencode failed: {}", e);
                        self.feishu
                            .reply_text(message_id, &format!("重启 OpenCode 失败：{}", e))
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

    /// `/switch <keyword>` — switch within the thread first, then adopt a unique
    /// global match (ADR-0008). Resolution order:
    /// 1. Current thread's mapped sessions (matched by title/directory/id);
    ///    a unique hit switches without changing the mapping.
    /// 2. Global store search (sub-task children excluded); a unique hit adopts
    ///    into the current thread and becomes active.
    /// 3. Multiple hits: list up to 8 candidates and point at `/attach`.
    async fn handle_switch(
        self: &Arc<Self>,
        thread_key: &ThreadKey,
        keyword: &str,
        message_id: &str,
        kind: ConversationKind,
    ) -> crate::error::Result<()> {
        let sessions = self.cached_session_list().await?;
        let lower = keyword.to_lowercase();

        // 1. Current thread's mapped sessions first.
        let thread_ids: Vec<String> = {
            let store = self.sessions.lock().await;
            store
                .list_thread(thread_key)
                .into_iter()
                .map(|e| e.session_id.clone())
                .collect()
        };
        let thread_hits: Vec<&crate::opencode::SessionListInfo> = sessions
            .iter()
            .filter(|s| thread_ids.contains(&s.id) && matches_keyword(s, &lower))
            .collect();
        if thread_hits.len() == 1 {
            let hit = thread_hits[0];
            let mut store = self.sessions.lock().await;
            if let Some(entry) = store
                .list_thread(thread_key)
                .into_iter()
                .find(|e| e.session_id == hit.id)
                .cloned()
            {
                store.set_active(entry);
                store.persist()?;
            }
            self.feishu
                .reply_text(message_id, &format!("Switched to \"{}\".", hit.title))
                .await?;
            return Ok(());
        }
        if thread_hits.len() > 1 {
            let list = candidates_list("本会话匹配到多个，请用 `/attach <完整ID>` 指定：", &thread_hits);
            self.feishu.reply_text(message_id, &list).await?;
            return Ok(());
        }

        // 2. Global search, children excluded.
        let global_hits: Vec<&crate::opencode::SessionListInfo> = sessions
            .iter()
            .filter(|s| !s.is_child() && matches_keyword(s, &lower))
            .collect();
        if global_hits.len() == 1 {
            let hit = global_hits[0].clone();
            self.adopt_session(thread_key, &hit, message_id, kind, false)
                .await?;
            return Ok(());
        }
        if global_hits.len() > 1 {
            let list = candidates_list("找到多个会话，请用 `/attach <完整ID>` 指定：", &global_hits);
            self.feishu.reply_text(message_id, &list).await?;
            return Ok(());
        }
        self.feishu
            .reply_text(message_id, &format!("No session matching \"{}\"", keyword))
            .await?;
        Ok(())
    }

    /// `/list [keyword] [--all]` — a cached, recently-active list of every
    /// session in the shared store (ADR-0008), so sessions created outside
    /// Feishu become visible. Sorted by last activity (client-side), capped at
    /// 15; children and archived hidden unless `--all`.
    async fn handle_list(
        self: &Arc<Self>,
        thread_key: &ThreadKey,
        keyword: Option<&str>,
        all: bool,
        message_id: &str,
    ) -> crate::error::Result<()> {
        let sessions = self.cached_session_list().await?;
        let lower = keyword.map(|k| k.to_lowercase());
        let mut shown: Vec<crate::opencode::SessionListInfo> = sessions
            .into_iter()
            .filter(|s| {
                if !all && (s.is_child() || s.time.as_ref().map(|t| t.is_archived()).unwrap_or(false)) {
                    return false;
                }
                match &lower {
                    Some(l) => matches_keyword(s, l),
                    None => true,
                }
            })
            .collect();
        shown.sort_by(|a, b| {
            let ub = b.time.as_ref().map(|t| t.updated).unwrap_or(0);
            let ua = a.time.as_ref().map(|t| t.updated).unwrap_or(0);
            ub.cmp(&ua)
        });
        shown.truncate(15);

        if shown.is_empty() {
            self.feishu
                .reply_text(message_id, "No sessions matching the filter.")
                .await?;
            return Ok(());
        }

        let (active_id, mapped_ids) = {
            let store = self.sessions.lock().await;
            let active = store.get_active(thread_key).map(|e| e.session_id.clone());
            let mapped: Vec<String> = store
                .list_thread(thread_key)
                .into_iter()
                .map(|e| e.session_id.clone())
                .collect();
            (active, mapped)
        };
        let mut list = String::from("**Recent sessions:**\n");
        for s in &shown {
            let mark = if active_id.as_deref() == Some(&s.id) {
                " (active)"
            } else if mapped_ids.contains(&s.id) {
                " (本会话)"
            } else {
                ""
            };
            let rel = s
                .time
                .as_ref()
                .map(|t| relative_time(t.updated))
                .unwrap_or_default();
            list.push_str(&format!(
                "- {} · {} · {}{}\n  {}\n",
                title_or_id_tail(s),
                s.directory,
                id_tail(&s.id),
                mark,
                rel
            ));
        }
        if !all {
            list.push_str("\n`/list --all` 显示子任务会话（当前隐藏）。");
        }
        self.feishu.reply_text(message_id, &list).await?;
        Ok(())
    }

    /// `/attach <id|title> [--force]` — take over an arbitrary server session
    /// into the current thread (ADR-0008). Resolution: exact id → unique
    /// id-prefix → unique title substring; multiple hits list candidates.
    async fn handle_attach(
        self: &Arc<Self>,
        thread_key: &ThreadKey,
        query: &str,
        force: bool,
        message_id: &str,
        kind: ConversationKind,
    ) -> crate::error::Result<()> {
        let sessions = self.cached_session_list().await?;
        let lower = query.to_lowercase();

        // Exact id.
        if let Some(s) = sessions.iter().find(|s| s.id == query) {
            return self.adopt_session(thread_key, s, message_id, kind, force).await;
        }
        // Unique id-prefix.
        let prefix: Vec<&crate::opencode::SessionListInfo> = sessions
            .iter()
            .filter(|s| s.id.to_lowercase().starts_with(&lower))
            .collect();
        if prefix.len() == 1 {
            return self
                .adopt_session(thread_key, prefix[0], message_id, kind, force)
                .await;
        }
        if prefix.len() > 1 {
            let list = candidates_list("ID 前缀匹配到多个，请用完整 ID：", &prefix);
            self.feishu.reply_text(message_id, &list).await?;
            return Ok(());
        }
        // Unique title substring.
        let titles: Vec<&crate::opencode::SessionListInfo> = sessions
            .iter()
            .filter(|s| s.title.to_lowercase().contains(&lower))
            .collect();
        if titles.len() == 1 {
            return self
                .adopt_session(thread_key, titles[0], message_id, kind, force)
                .await;
        }
        if titles.len() > 1 {
            let list = candidates_list("标题匹配到多个，请用完整 ID：", &titles);
            self.feishu.reply_text(message_id, &list).await?;
            return Ok(());
        }
        self.feishu
            .reply_text(message_id, &format!("No session matching \"{}\"", query))
            .await?;
        Ok(())
    }

    /// Adopt a server session as the current thread's session, honoring the
    /// one-session-one-thread invariant (ADR-0007). Already the active session
    /// → idempotent no-op. Mapped to another thread → rejected with an
    /// actionable card unless `--force` (which steals the mapping). Copies
    /// `directory` + `agent` from the server; `auto_accept` resets to false.
    /// In a never-had-a-session topic, the fallback-card anchor is the
    /// command's own reply inside the topic (`reply_in_thread`, ADR-0006).
    async fn adopt_session(
        self: &Arc<Self>,
        thread_key: &ThreadKey,
        info: &crate::opencode::SessionListInfo,
        message_id: &str,
        kind: ConversationKind,
        force: bool,
    ) -> crate::error::Result<()> {
        // Idempotent: already the active session of this thread.
        {
            let store = self.sessions.lock().await;
            if let Some(e) = store.get_active(thread_key)
                && e.session_id == info.id
            {
                self.feishu
                    .reply_text(message_id, &format!("Already active: \"{}\".", info.title))
                    .await?;
                return Ok(());
            }
        }
        // Mapped to another thread: reject unless --force.
        let owner = {
            let store = self.sessions.lock().await;
            store.thread_for_session(&info.id)
        };
        if let Some(owner_key) = owner
            && owner_key != *thread_key
        {
            if !force {
                let chat_name = self
                    .feishu
                    .chat_name(&owner_key.chat_id)
                    .await
                    .unwrap_or(None)
                    .unwrap_or_else(|| owner_key.chat_id.clone());
                let where_flag = if owner_key.thread_id != owner_key.chat_id {
                    "话题"
                } else {
                    "主对话"
                };
                self.feishu
                    .reply_text(
                        message_id,
                        &format!(
                            "⚠️ 会话 `{}`（目录 `{}`）已被其他聊天占用：\n{}\n（{}，chat `{}`）\n\n可先请对方 `/forget` 解除，或使用 `/attach {} --force` 强行接管。",
                            info.title,
                            info.directory,
                            chat_name,
                            where_flag,
                            owner_key.chat_id,
                            info.id
                        ),
                    )
                    .await?;
                return Ok(());
            }
            // --force: steal the mapping; the other thread becomes sessionless.
            let mut store = self.sessions.lock().await;
            store.remove(&info.id);
            store.persist()?;
        }

        let anchor = if kind == ConversationKind::Topic {
            match self
                .feishu
                .reply_in_thread(
                    message_id,
                    &format!("📎 已接管会话 `{}`（目录 `{}`）。", info.title, info.directory),
                )
                .await
            {
                Ok((anchor, _)) => Some(anchor),
                Err(e) => {
                    tracing::warn!("attach: reply_in_thread failed: {}", e);
                    None
                }
            }
        } else {
            None
        };
        let entry = SessionEntry {
            thread_key: thread_key.clone(),
            session_id: info.id.clone(),
            directory: info.directory.clone(),
            agent: info.agent.clone(),
            auto_accept: false,
            topic_anchor: anchor,
        };
        {
            let mut store = self.sessions.lock().await;
            store.set_active(entry);
            store.persist()?;
        }
        self.invalidate_session_list_cache().await;
        // In a topic the confirmation was already sent inside it (the
        // `reply_in_thread` above doubles as the fallback-card anchor); don't
        // reply twice.
        if kind != ConversationKind::Topic {
            self.feishu
                .reply_text(
                    message_id,
                    &format!("已接管会话 `{}`（目录 `{}`）。", info.title, info.directory),
                )
                .await?;
        }
        Ok(())
    }
}

/// The last 7 characters of a session id (display suffix).
fn id_tail(id: &str) -> String {
    id.strip_prefix("ses_").unwrap_or(id).chars().take(7).collect()
}

/// Case-insensitive keyword match on a session's title, directory or id
/// (shared by `/list`, `/switch` and `/attach`).
fn matches_keyword(s: &crate::opencode::SessionListInfo, lower: &str) -> bool {
    s.title.to_lowercase().contains(lower)
        || s.directory.to_lowercase().contains(lower)
        || s.id.to_lowercase().contains(lower)
}

/// The "ambiguous match" candidate list shown when a keyword or id resolves to
/// several sessions: `title · dir · id-tail`, capped at 8.
fn candidates_list(header: &str, sessions: &[&crate::opencode::SessionListInfo]) -> String {
    let mut list = String::from(header);
    for s in sessions.iter().take(8) {
        list.push_str(&format!("- {} · {} · {}\n", s.title, s.directory, id_tail(&s.id)));
    }
    list
}

/// A session's display title, falling back to the id-tail when the title is a
/// meaningless default (`New session - ...`, `sess-<uuid>`, etc.).
fn title_or_id_tail(s: &crate::opencode::SessionListInfo) -> String {
    let cleaned = crate::feishu::card::clean_session_label(&s.title);
    if cleaned.is_empty() {
        id_tail(&s.id)
    } else {
        cleaned
    }
}

/// Compact relative-time label for a millisecond timestamp.
fn relative_time(ms: i64) -> String {
    let now = chrono::Utc::now().timestamp_millis();
    let secs = ((now - ms) / 1000).max(0);
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86400)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_is_not_command() {
        assert_eq!(parse_command("hello world"), None);
        assert_eq!(parse_command("fix the bug"), None);
    }

    #[test]
    fn parse_dir_with_path() {
        let cmd = parse_command("/dir /home/user/project");
        assert_eq!(cmd, Some(Command::Dir("/home/user/project".into())));
    }

    #[test]
    fn parse_dir_missing_arg_shows_dir_help() {
        assert_eq!(parse_command("/dir"), Some(Command::Help(Some("dir".into()))));
        assert_eq!(parse_command("/name"), Some(Command::Help(Some("name".into()))));
        assert_eq!(parse_command("/model"), Some(Command::Help(Some("model".into()))));
        assert_eq!(parse_command("/agent"), Some(Command::Help(Some("agent".into()))));
        assert_eq!(
            parse_command("/switch"),
            Some(Command::Help(Some("switch".into())))
        );
    }

    #[test]
    fn parse_switch() {
        assert_eq!(
            parse_command("/switch backend"),
            Some(Command::Switch("backend".into()))
        );
    }

    #[test]
    fn parse_list() {
        assert_eq!(
            parse_command("/list"),
            Some(Command::List {
                keyword: None,
                all: false
            })
        );
        assert_eq!(
            parse_command(" /list "),
            Some(Command::List {
                keyword: None,
                all: false
            })
        );
    }

    #[test]
    fn parse_list_keyword_and_all() {
        assert_eq!(
            parse_command("/list cola"),
            Some(Command::List {
                keyword: Some("cola".into()),
                all: false
            })
        );
        assert_eq!(
            parse_command("/list --all"),
            Some(Command::List {
                keyword: None,
                all: true
            })
        );
        assert_eq!(
            parse_command("/list cola --all"),
            Some(Command::List {
                keyword: Some("cola".into()),
                all: true
            })
        );
        assert_eq!(
            parse_command("/list multi word"),
            Some(Command::List {
                keyword: Some("multi word".into()),
                all: false
            })
        );
    }

    #[test]
    fn parse_attach() {
        assert_eq!(
            parse_command("/attach ses_abc123"),
            Some(Command::Attach {
                query: "ses_abc123".into(),
                force: false
            })
        );
        assert_eq!(
            parse_command("/attach ses_abc123 --force"),
            Some(Command::Attach {
                query: "ses_abc123".into(),
                force: true
            })
        );
        assert_eq!(
            parse_command("/attach some title"),
            Some(Command::Attach {
                query: "some title".into(),
                force: false
            })
        );
        assert_eq!(
            parse_command("/attach"),
            Some(Command::Help(Some("attach".into())))
        );
    }

    #[test]
    fn parse_forget() {
        assert_eq!(parse_command("/forget"), Some(Command::Forget));
    }

    #[test]
    fn parse_restart() {
        assert_eq!(parse_command("/restart"), Some(Command::Restart));
        assert_eq!(parse_command("/restart now"), Some(Command::Restart));
    }

    #[test]
    fn parse_new_with_name() {
        assert_eq!(
            parse_command("/new my-session"),
            Some(Command::New(Some("my-session".into())))
        );
    }

    #[test]
    fn parse_new_without_name() {
        assert_eq!(parse_command("/new"), Some(Command::New(None)));
    }

    #[test]
    fn parse_topic_with_dir_and_name() {
        assert_eq!(
            parse_command("/topic /root/proj/lib api-refactor"),
            Some(Command::Topic {
                directory: "/root/proj/lib".into(),
                name: Some("api-refactor".into()),
            })
        );
    }

    #[test]
    fn parse_topic_with_dir_only() {
        assert_eq!(
            parse_command("/topic /root/proj/lib"),
            Some(Command::Topic {
                directory: "/root/proj/lib".into(),
                name: None,
            })
        );
    }

    #[test]
    fn parse_topic_missing_arg_shows_help() {
        assert_eq!(parse_command("/topic"), Some(Command::Help(Some("topic".into()))));
        assert_eq!(
            parse_command("/topic "),
            Some(Command::Help(Some("topic".into())))
        );
    }

    #[test]
    fn parse_name() {
        assert_eq!(
            parse_command("/name frontend-refactor"),
            Some(Command::Name("frontend-refactor".into()))
        );
    }

    #[test]
    fn parse_stop() {
        assert_eq!(parse_command("/stop"), Some(Command::Stop));
    }

    #[test]
    fn parse_compact() {
        assert_eq!(parse_command("/compact"), Some(Command::Compact));
    }

    #[test]
    fn parse_agent() {
        assert_eq!(
            parse_command("/agent primary"),
            Some(Command::Agent("primary".into()))
        );
    }

    #[test]
    fn parse_model() {
        assert_eq!(
            parse_command("/model anthropic/claude-sonnet-4-5"),
            Some(Command::Model("anthropic/claude-sonnet-4-5".into()))
        );
    }

    #[test]
    fn parse_help() {
        assert_eq!(parse_command("/help"), Some(Command::Help(None)));
        assert_eq!(
            parse_command("/help model"),
            Some(Command::Help(Some("model".into())))
        );
        assert_eq!(
            parse_command("/help Model"),
            Some(Command::Help(Some("model".into())))
        );
    }

    #[test]
    fn command_help_known_and_unknown() {
        assert!(command_help("model").unwrap().contains("/model"));
        assert!(command_help("dir").unwrap().contains("NEW session"));
        assert_eq!(command_help("nonexistent"), None);
    }

    #[test]
    fn unknown_command_forwarded() {
        let cmd = parse_command("/init");
        assert_eq!(cmd, Some(Command::Forward("/init".into())));

        let cmd2 = parse_command("/some-unknown-command arg1");
        assert_eq!(cmd2, Some(Command::Forward("/some-unknown-command arg1".into())));
    }

    #[test]
    fn trailing_spaces_ignored() {
        assert_eq!(
            parse_command("  /list  "),
            Some(Command::List {
                keyword: None,
                all: false
            })
        );
    }

    #[test]
    fn case_insensitive_command() {
        assert_eq!(parse_command("/STOP"), Some(Command::Stop));
        assert_eq!(
            parse_command("/List"),
            Some(Command::List {
                keyword: None,
                all: false
            })
        );
    }
}
