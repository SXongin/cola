/// Parsed slash command from user message text.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// Change project directory for the current session
    Dir(String),
    /// Session management (`/switch ...`): match, list, forget, adopt, or the
    /// interactive card. Absorbed the old `/list`, `/attach`, `/forget`.
    Switch(SwitchAction),
    /// Create a fresh session, optionally named
    New(Option<String>),
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
    /// `/agent` (no args) — interactive agent-picker card.
    AgentCard,
    /// Switch model in the current session
    Model(String),
    /// `/model` (no args) — interactive model-picker card.
    ModelCard,
    /// Auto-accept of permission requests for the current session.
    AutoAccept(AutoAcceptAction),
    /// Restart cola itself, preserving startup args and the log redirect.
    Restart,
    /// Restart the OpenCode server — but only when cola started it. A server
    /// another tool launched is never touched.
    RestartOpenCode,
    /// Self-update from GitHub Releases (ADR-0015): check, download, verify,
    /// replace the running binary, and restart.
    Update,
    /// Show available commands, or help for one command (`/help <cmd>`).
    Help(Option<String>),
    /// Forward unrecognized slash command to OpenCode as prompt text
    Forward(String),
}

/// What `/switch` should do (ADR-0012). The text-direct forms all share the
/// session store; the no-arg form pops the interactive card.
#[derive(Debug, Clone, PartialEq)]
pub enum SwitchAction {
    /// `/switch` (no args) — the interactive session card (issue 04).
    Card,
    /// `/switch <keyword>` — matching rules: thread's sessions first, then a
    /// unique global match is adopted (ADR-0008).
    Match(String),
    /// `/switch list [keyword] [--all]` — the old `/list`.
    List { keyword: Option<String>, all: bool },
    /// `/switch forget` — the old `/forget`.
    Forget,
    /// `/switch <id|title> [--force]` — the old `/attach`.
    Attach { query: String, force: bool },
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
            // `/switch` (no args) — the interactive session card.
            None => Some(Command::Switch(SwitchAction::Card)),
            Some(a) => {
                let mut words: Vec<&str> = Vec::new();
                let mut force = false;
                for w in a.split_whitespace() {
                    if w == "--force" {
                        force = true;
                    } else {
                        words.push(w);
                    }
                }
                // `/switch list [keyword] [--all]` — the old `/list`.
                if words.first().map(|w| w.to_lowercase()) == Some("list".into()) {
                    let mut keyword = None;
                    let mut all = false;
                    for w in words.iter().skip(1) {
                        if *w == "--all" {
                            all = true;
                        } else {
                            keyword =
                                Some(keyword.map_or_else(|| w.to_string(), |k: String| format!("{k} {w}")));
                        }
                    }
                    return Some(Command::Switch(SwitchAction::List { keyword, all }));
                }
                // `/switch forget` — the old `/forget`.
                if words.first().map(|w| w.to_lowercase()) == Some("forget".into()) && words.len() == 1 {
                    return Some(Command::Switch(SwitchAction::Forget));
                }
                let query = words.join(" ");
                if query.is_empty() {
                    Some(Command::Switch(SwitchAction::Card))
                } else if force {
                    Some(Command::Switch(SwitchAction::Attach { query, force }))
                } else {
                    Some(Command::Switch(SwitchAction::Match(query)))
                }
            }
        },
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
            None => Some(Command::AgentCard),
        },
        "/model" => match arg {
            Some(p) => Some(Command::Model(p.to_string())),
            None => Some(Command::ModelCard),
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
        "/update" => Some(Command::Update),
        "/help" => Some(Command::Help(arg.map(|s| s.to_lowercase()))),
        // `/init`, `/review`, or any unknown /command — forward to OpenCode
        _ => Some(Command::Forward(trimmed.to_string())),
    }
}

/// Help text shown for the `/help` command.
pub fn help_text() -> String {
    "\
**cola commands**
`/dir <path> [name]` · Switch to a project + new session there
`/switch` · Session card: browse / search / adopt / new
`/switch <kw>` · Switch to a session by name/dir/id (adopts foreign ones)
`/switch list [kw] [--all]` · List recent sessions across the store
`/switch <id> [--force]` · Take over a session by id/title
`/switch forget` · Un-map this chat's session (server session stays)
`/new [name]` · New session in the current project (no session → default dir)
`/topic <dir> [name]` · Create a new Feishu topic + session in <dir>
`/name <name>` · Rename current session (server-side)
`/stop` · Interrupt execution
`/compact` · Compact context
`/agent <name>` · Switch agent (takes effect next message)
`/model <p/m>` · Switch model (takes effect next message)
`/autoaccept` · Show auto-approve status; `/autoaccept on|off` switches
`/restart` · Restart cola (keeps startup args + log redirect)
`/restart-opencode` · Restart the OpenCode server (only when cola started it)
`/update` · Check for and apply a cola self-update from GitHub Releases
`/help <command>` · Show help for one command (e.g. `/help model`)

话题规则：已绑定会话的话题里，`/switch`、`/new`、`/dir` 被拒绝，请回主对话操作。从未绑定过会话的话题可以用它们来绑定该话题的唯一会话。
    "
    .to_string()
}

/// Detailed help for one command (`/help <command>`). `None` for unknown names.
pub fn command_help(name: &str) -> Option<String> {
    let text = match name.to_lowercase().as_str() {
        "dir" => {
            "/dir <path> [name]\nSwitch to a project: open a NEW session rooted at <path> (create a session rooted at that directory).\nExample: `/dir /root/proj/lib`"
        }
        "switch" => {
            "/switch [action]\nSession management card and text forms.\n- `/switch` (no arg) — interactive session card (browse / search / adopt / new)\n- `/switch <keyword>` — switch by title/directory/id; the current chat's sessions win, otherwise a unique global match is adopted. Ambiguous keywords list candidates.\n- `/switch list [keyword] [--all]` — list recent sessions across the store (up to 15)\n- `/switch <id|title> [--force]` — take over a session (exact id → id-prefix → title; reject if owned by another chat unless `--force`)\n- `/switch forget` — un-map this chat's session (server session stays)\nExamples: `/switch backend`, `/switch list cola`, `/switch ses_abc --force`"
        }
        "list" => {
            "/switch list [keyword] [--all]\nList recently-active sessions across the shared store (up to 15): title, directory, id and last activity. A keyword filters by title/directory/id; `--all` also shows sub-task child sessions.\nExample: `/switch list cola`"
        }
        "attach" => {
            "/switch <id|title> [--force]\nTake over a session created outside Feishu into this chat. Resolution: exact id → unique id-prefix → unique title substring. If the session already belongs to another chat, show its owner and reject unless `--force`.\nExample: `/switch ses_abc123`"
        }
        "forget" => {
            "/switch forget\nUn-map this chat's session. The server session stays untouched and can be adopted again.\nExample: `/switch forget`"
        }
        "new" => {
            "/new [name]\nCreate a fresh session in the current project (the active session's directory); with no active session, the default directory (`work_dir` or cwd). Optionally named (the name is PATCHed server-side); without a name the server generates one after the first message.\nExample: `/new api-refactor`"
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
        "agent" => {
            "/agent <name>\nSwitch the agent for the current session — a per-session override sent on the NEXT message (the OpenCode server has no agent-switch endpoint; the session's own/default agent otherwise). Persisted across restarts. Unknown agent names surface as an error on the next prompt.\nExample: `/agent build`"
        }
        "model" => {
            "/model <provider/model>\nSwitch the model for the current session — a per-session override sent on the NEXT message (the server has no model-switch endpoint; unset = the configured default / server default). Persisted across restarts.\nExample: `/model opencode-go/deepseek-v4-flash`"
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
        "update" => {
            "/update\nCheck GitHub Releases for a newer cola; if one exists, download, verify (SHA256SUMS), replace the running binary and restart. When running as a systemd unit, the restart hands back to `Restart=on-failure`; otherwise the new process re-execs with --replace. If already on the latest version, it reports that and does nothing."
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

use crate::bridge::core::SharedCore;
use crate::config::{ConversationKind, SessionEntry, ThreadKey};
use crate::feishu;
use std::sync::Arc;

/// Re-exec cola itself with the ORIGINAL startup args, inheriting stdio so a
/// shell log redirect (`cola ... > test.log 2>&1`) carries into the new process.
/// `--replace` is appended so the new process can take over the singleton lock
/// even while the old process lingers (briefly alive, then a zombie until the
/// launching shell reaps it). The current process then calls
/// `std::process::exit(0)` right after.
pub(crate) fn restart_process() -> std::io::Result<()> {
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

/// Report self-update progress as Feishu text replies (ADR-0015).
struct FeishuUpdateReporter<'a> {
    feishu: &'a Arc<dyn crate::feishu::Platform>,
    message_id: &'a str,
}

#[async_trait::async_trait]
impl<'a> crate::update::UpdateReporter for FeishuUpdateReporter<'a> {
    async fn report(&self, msg: String) {
        let _ = self.feishu.reply_text(self.message_id, &msg).await;
    }
}

/// Execute a parsed slash command against the shared core. Unrecognized
/// `/command`s are intercepted by the message coordinator (which owns the
/// prompt pipeline), so this never forwards — the `Command::Forward` arm below
/// is unreachable and kept only for exhaustiveness.
pub(crate) async fn handle_command(
    core: &Arc<SharedCore>,
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
        let blocked = matches!(cmd, Command::Dir(_) | Command::Switch(_) | Command::New(_));
        if blocked {
            let has_session = core.sessions.lock().await.get_active(&thread_key).is_some();
            if has_session {
                core.feishu
                    .reply_text(message_id, "⚠️ 话题已绑定会话，请回主对话操作。")
                    .await?;
                return Ok(());
            }
        }
    }
    match cmd {
        Command::Dir(path) => {
            let Some(dir_str) = resolve_directory_or_reply(
                &*core.feishu,
                message_id,
                &path,
                "`/dir`；或先用 `/new` 在默认目录新建会话。",
            )
            .await?
            else {
                return Ok(());
            };
            // `/dir` opens a NEW conversation rooted at `path` (matching
            // OpenCode's per-directory sessions), not a rename of the old
            // one. Create the session first so a failure is reported here
            // instead of as a cryptic card error on the next message.
            let session = match core
                .opencode
                .create_session(&core.opencode.new_session_input(Some(&dir_str)))
                .await
            {
                Ok(s) => s,
                Err(e) => {
                    core.feishu
                        .reply_text(
                            message_id,
                            &format!("⚠️ 创建会话失败（目录 `{}`）：{}", dir_str, e),
                        )
                        .await?;
                    return Ok(());
                }
            };
            let entry = SessionEntry {
                thread_key: thread_key.clone(),
                session_id: session.id.clone(),
                directory: dir_str.clone(),
                agent: None,
                model: None,
                auto_accept: false,
                topic_anchor: None,
            };
            let mut store = core.sessions.lock().await;
            store.set_active(entry);
            store.persist()?;
            core.invalidate_session_list_cache().await;
            core.feishu
                .reply_text(
                    message_id,
                    &format!(
                        "已切换目录并新建会话（目录 `{}`）。\n后续对话都会在这个目录下进行。",
                        dir_str
                    ),
                )
                .await?;
        }
        Command::Switch(action) => {
            handle_switch_action(core, &thread_key, action, message_id, kind).await?;
        }
        Command::New(name) => {
            // The current project follows the active session (ADR-0012): `/new`
            // stays in the project the user is already working in. Only when
            // the conversation has no session (fresh topic, after `/forget`,
            // adopted-away) does it fall back to the default directory.
            let directory = {
                let store = core.sessions.lock().await;
                store
                    .get_active(&thread_key)
                    .map(|e| e.directory.clone())
                    .filter(|d| !d.is_empty())
                    .unwrap_or_else(|| core.default_session_directory())
            };
            let session = core
                .opencode
                .create_session(&core.opencode.new_session_input(Some(&directory)))
                .await?;
            // Creation title policy (ADR-0007): `/new <name>` PATCHes the
            // title immediately; `/new` (no name) leaves the server default
            // so a title is auto-generated after the first message.
            if let Some(n) = &name {
                core.opencode.update_session_title(&session.id, n).await?;
            }
            let entry = SessionEntry {
                thread_key: thread_key.clone(),
                session_id: session.id.clone(),
                directory,
                agent: None,
                model: None,
                auto_accept: false,
                topic_anchor: None,
            };
            let mut store = core.sessions.lock().await;
            store.set_active(entry);
            store.persist()?;
            core.invalidate_session_list_cache().await;
            let label = name.unwrap_or_else(|| format!("sess-{}", uuid::Uuid::new_v4()));
            core.feishu
                .reply_text(message_id, &format!("Created \"{}\".", label))
                .await?;
        }
        Command::Topic { directory, name } => {
            // Opening a topic from inside another topic would nest
            // confusingly; only create topics from a non-topic message.
            if kind == ConversationKind::Topic {
                core.feishu
                    .reply_text(
                        message_id,
                        "⚠️ /topic 只能从会话顶层使用，不能在话题里再开话题。请在主会话里发 /topic <目录>。",
                    )
                    .await?;
                return Ok(());
            }
            let Some(dir_str) =
                resolve_directory_or_reply(&*core.feishu, message_id, &directory, "`/topic`。").await?
            else {
                return Ok(());
            };
            let session = core
                .opencode
                .create_session(&core.opencode.new_session_input(Some(&dir_str)))
                .await?;
            // Creation title policy (ADR-0007): a named `/topic` PATCHes the
            // title; without a name the server default is left for
            // auto-generation. The display name only drives the anchor text.
            if let Some(n) = &name {
                core.opencode.update_session_title(&session.id, n).await?;
            }
            let display_name = name.unwrap_or_else(|| {
                dir_str
                    .trim_end_matches('/')
                    .rsplit('/')
                    .next()
                    .filter(|s| !s.is_empty())
                    .unwrap_or(&dir_str)
                    .to_string()
            });
            // Create a real topic anchored on the user's command message.
            // `anchor` is the created reply's own message_id — a message
            // INSIDE the topic, so permission/question/external cards that
            // must be sent (no streaming card) can reply to it and stay in
            // the topic (the create API rejects `thread_id` as a target).
            let (anchor, thread_id) = core
                .feishu
                .reply_in_thread(
                    message_id,
                    &format!(
                        "📌 已创建会话 `{}`（目录 `{}`）。\n请在本话题内回复，即可和这个会话对话。",
                        display_name, dir_str
                    ),
                )
                .await?;
            let Some(thread_id) = thread_id else {
                tracing::warn!(
                    "topic: no thread_id returned for /topic in chat {}; not mapping session",
                    thread_key.chat_id
                );
                core.feishu
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
                directory: dir_str,
                agent: None,
                model: None,
                auto_accept: false,
                topic_anchor: Some(anchor),
            };
            let mut store = core.sessions.lock().await;
            store.set_active(entry);
            store.persist()?;
            core.invalidate_session_list_cache().await;
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
            if let Some(id) = core.get_session_id(&thread_key).await {
                core.opencode.update_session_title(&id, &name).await?;
                core.invalidate_session_list_cache().await;
            }
            core.feishu
                .reply_text(message_id, &format!("Renamed to \"{}\".", name))
                .await?;
        }
        Command::AutoAccept(action) => {
            // `Status` reports the current state; `Set(on)` switches the flag
            // AND clears requests that are already pending but were seen
            // before (the poller's `seen` set skips them, so they'd
            // otherwise hang as cards forever).
            let entry = {
                let store = core.sessions.lock().await;
                store.get_active(&thread_key).cloned()
            };
            match action {
                crate::bridge::command::AutoAcceptAction::Status => {
                    send_autoaccept_card(core, &thread_key, message_id).await?;
                    return Ok(());
                }
                crate::bridge::command::AutoAcceptAction::Set(on) => {
                    let approved = if on {
                        if let Some(e) = &entry {
                            core.approve_pending_for_session(&e.session_id, &e.directory)
                                .await
                        } else {
                            0
                        }
                    } else {
                        0
                    };
                    if let Some(e) = entry {
                        let mut store = core.sessions.lock().await;
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
                    core.feishu
                        .reply_text(message_id, &format!("🔁 已将会话自动审批{state}。{}", extra))
                        .await?;
                }
            }
        }
        Command::Stop => {
            if let Some(id) = core.get_session_id(&thread_key).await {
                core.opencode.interrupt(&id).await?;
                core.feishu.reply_text(message_id, "Interrupted.").await?;
            } else {
                core.feishu
                    .reply_text(message_id, "当前没有正在执行的会话。")
                    .await?;
            }
        }
        Command::Compact => {
            if let Some(id) = core.get_session_id(&thread_key).await {
                core.opencode.compact(&id).await?;
                core.feishu.reply_text(message_id, "Compacting...").await?;
            } else {
                core.feishu
                    .reply_text(message_id, "当前对话还没有会话，无需压缩。")
                    .await?;
            }
        }
        Command::AgentCard => {
            send_agent_card(core, &thread_key, message_id).await?;
        }
        Command::Agent(name) => {
            // The OpenCode server has no agent-switch endpoint (the legacy
            // `/api/session/{id}/agent` route 500s, same as `/model`'s dead
            // route), so `/agent` records a per-session override here — persisted
            // in the SessionEntry so it survives a restart — and cola sends it as
            // a per-prompt agent on the next message (the server honors
            // `PromptInput.agent`). Unknown agent names surface as a clear error
            // on the next prompt's card.
            let entry = {
                let store = core.sessions.lock().await;
                store.get_active(&thread_key).cloned()
            };
            let Some(mut entry) = entry else {
                core.feishu
                    .reply_text(message_id, "⚠️ 当前对话还没有会话，先用 `/new` 或 `/dir` 创建。")
                    .await?;
                return Ok(());
            };
            entry.agent = Some(name.clone());
            {
                let mut store = core.sessions.lock().await;
                store.set_active(entry);
                store.persist()?;
            }
            core.feishu
                .reply_text(message_id, &format!("Agent: {}（下一条消息开始生效）", name))
                .await?;
        }
        Command::ModelCard => {
            send_model_card(core, &thread_key, message_id).await?;
        }
        Command::Model(name) => {
            // The OpenCode server has NO model-switch endpoint (the legacy
            // `/api/session/{id}/model` route is gone), so `/model` records
            // a per-session override here and cola sends it as a per-prompt
            // model on the next message. Validate the shape up front so a
            // typo gets immediate feedback instead of a silent no-op. The
            // override is persisted in the SessionEntry (survives restart).
            let Some(_) = crate::opencode::client::parse_model(&name) else {
                core.feishu
                        .reply_text(
                            message_id,
                            &format!(
                                "⚠️ 模型格式应为 `<provider>/<model>`，例如 `/model opencode-go/deepseek-v4-flash`。\n收到：`{}`",
                                name
                            ),
                        )
                        .await?;
                return Ok(());
            };
            let Some(mut entry) = core.sessions.lock().await.get_active(&thread_key).cloned() else {
                core.feishu
                    .reply_text(message_id, "⚠️ 当前对话还没有会话，先用 `/new` 或 `/dir` 创建。")
                    .await?;
                return Ok(());
            };
            entry.model = Some(name.clone());
            {
                let mut store = core.sessions.lock().await;
                store.set_active(entry);
                store.persist()?;
            }
            core.feishu
                .reply_text(message_id, &format!("Model: {}（下一条消息开始生效）", name))
                .await?;
        }
        Command::Help(target) => {
            match target {
                // No target: the interactive navigation card (ADR-0012, issue 05).
                None => {
                    send_help_card(core, &thread_key, message_id).await?;
                }
                Some(name) => {
                    let text = match command_help(&name) {
                        Some(h) => h,
                        None => format!("未知命令 `{}`。\n\n{}", name, help_text()),
                    };
                    core.feishu.reply_text(message_id, &text).await?;
                }
            }
        }
        Command::Restart => {
            // Reply BEFORE exiting, then re-exec ourselves with the SAME
            // startup args and inherited stdio (so the log redirect to
            // test.log keeps working in the new process).
            core.feishu.reply_text(message_id, "♻️ 正在重启，稍候…").await?;
            // Remember which chat to announce the restart in.
            let notify = serde_json::json!({ "chat_id": thread_key.chat_id });
            let _ = std::fs::write(restart_notify_path(), notify.to_string());
            match restart_process() {
                Ok(()) => std::process::exit(0),
                Err(e) => {
                    tracing::error!("restart spawn failed: {}", e);
                    core.feishu
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
                    core.feishu
                        .reply_text(message_id, "♻️ 已重启 OpenCode 服务器。")
                        .await?;
                }
                Ok(crate::bridge::discovery::RestartOutcome::NotOwned) => {
                    core.feishu
                        .reply_text(
                            message_id,
                            "这个 OpenCode 服务器不是 cola 启动的，需要你手动重启它。",
                        )
                        .await?;
                }
                Ok(crate::bridge::discovery::RestartOutcome::NoServer) => {
                    core.feishu
                        .reply_text(message_id, "当前没有正在运行的 OpenCode 服务器。")
                        .await?;
                }
                Err(e) => {
                    tracing::error!("restart opencode failed: {}", e);
                    core.feishu
                        .reply_text(message_id, &format!("重启 OpenCode 失败：{}", e))
                        .await?;
                }
            }
        }
        Command::Update => {
            // Self-update (ADR-0015): progress reports come back as text
            // replies; on success write the announce file (the new process
            // announces "已更新到 X" in this chat) and restart.
            let reporter = FeishuUpdateReporter {
                feishu: &core.feishu,
                message_id,
            };
            if let crate::update::UpdateOutcome::Updated(new_version) =
                crate::update::run_update(&reporter, crate::update::UpdateMode::Apply).await
            {
                core.feishu.reply_text(message_id, "正在重启…").await?;
                let notify = serde_json::json!({
                    "chat_id": thread_key.chat_id,
                    "kind": "update",
                    "version": new_version.to_string(),
                });
                let _ = std::fs::write(restart_notify_path(), notify.to_string());
                crate::update::restart();
            }
        }
        Command::Forward(_) => {
            // Unreachable: the message coordinator intercepts `Command::Forward`
            // and routes it into the prompt pipeline itself (that pipeline is the
            // coordinator's job, and command dispatch must not depend on it).
        }
    }
    Ok(())
}

/// Dispatch a `/switch` action (ADR-0012). The text-direct forms share the
/// old command handlers; the no-arg form pops the interactive session card.
async fn handle_switch_action(
    core: &Arc<SharedCore>,
    thread_key: &ThreadKey,
    action: SwitchAction,
    message_id: &str,
    kind: ConversationKind,
) -> crate::error::Result<()> {
    match action {
        SwitchAction::Card => {
            send_switch_card(core, thread_key, "", message_id).await?;
            Ok(())
        }
        SwitchAction::Match(keyword) => handle_switch(core, thread_key, &keyword, message_id, kind).await,
        SwitchAction::List { keyword, all } => {
            handle_list(core, thread_key, keyword.as_deref(), all, message_id).await
        }
        SwitchAction::Forget => {
            let mut store = core.sessions.lock().await;
            let removed = store.remove_thread(thread_key);
            store.persist()?;
            core.invalidate_session_list_cache().await;
            if removed.is_empty() {
                core.feishu.reply_text(message_id, "当前没有映射的会话。").await?;
            } else {
                core.feishu
                    .reply_text(
                        message_id,
                        "已解除本会话的映射（服务器会话仍保留，可用 `/switch list` 重新找到）。",
                    )
                    .await?;
            }
            Ok(())
        }
        SwitchAction::Attach { query, force } => {
            handle_attach(core, thread_key, &query, force, message_id, kind).await
        }
    }
}

/// Fetch + shape the data the `/switch` card renders: the session list
/// (children and archived excluded, filtered by `keyword`, sorted by last
/// activity) plus the thread's active + mapped session ids. Shared by the text
/// send path (`send_switch_card`) and the card ack refresh
/// (`App::build_switch_card_for`) so both render from one source of truth.
pub(crate) async fn switch_card_data(
    core: &Arc<SharedCore>,
    thread_key: &ThreadKey,
    keyword: &str,
) -> (Vec<crate::opencode::SessionListInfo>, Option<String>, Vec<String>) {
    let sessions = core.cached_session_list().await.unwrap_or_default();
    let lower = keyword.to_lowercase();
    let mut shown: Vec<crate::opencode::SessionListInfo> = sessions
        .into_iter()
        .filter(|s| {
            !s.is_child()
                && !s.time.as_ref().map(|t| t.is_archived()).unwrap_or(false)
                && if lower.is_empty() {
                    true
                } else {
                    matches_keyword(s, &lower)
                }
        })
        .collect();
    shown.sort_by(|a, b| {
        let ub = b.time.as_ref().map(|t| t.updated).unwrap_or(0);
        let ua = a.time.as_ref().map(|t| t.updated).unwrap_or(0);
        ub.cmp(&ua)
    });
    let (active_id, mapped_ids) = {
        let store = core.sessions.lock().await;
        let active = store.get_active(thread_key).map(|e| e.session_id.clone());
        let mapped: Vec<String> = store
            .list_thread(thread_key)
            .into_iter()
            .map(|e| e.session_id.clone())
            .collect();
        (active, mapped)
    };
    (shown, active_id, mapped_ids)
}

/// Build and send the interactive `/switch` session card (ADR-0012, issue 04).
/// Renders the filtered session list (via `switch_card_data`) and replies with
/// the card.
async fn send_switch_card(
    core: &Arc<SharedCore>,
    thread_key: &ThreadKey,
    keyword: &str,
    message_id: &str,
) -> crate::error::Result<()> {
    let (shown, active_id, mapped_ids) = switch_card_data(core, thread_key, keyword).await;
    let card = crate::feishu::card::build_switch_card(
        thread_key,
        &shown,
        keyword,
        active_id.as_deref(),
        &mapped_ids,
    );
    core.feishu.reply_card(message_id, &card).await?;
    Ok(())
}

/// Send the `/agent` picker card (ADR-0012, issue 05): one button per agent.
async fn send_agent_card(
    core: &Arc<SharedCore>,
    thread_key: &ThreadKey,
    message_id: &str,
) -> crate::error::Result<()> {
    let agents = core.opencode.list_agents().await;
    let card = crate::feishu::card::build_agent_card(thread_key, &agents);
    core.feishu.reply_card(message_id, &card).await?;
    Ok(())
}

/// Send the `/model` provider-picker cards (ADR-0012, issue 05): step 1 of a
/// two-level provider → model flow, chunked so any provider count stays under
/// Feishu's card limits.
async fn send_model_card(
    core: &Arc<SharedCore>,
    thread_key: &ThreadKey,
    message_id: &str,
) -> crate::error::Result<()> {
    let providers = core.opencode.list_models().await;
    let cards = crate::feishu::card::build_model_provider_cards(thread_key, &providers);
    for card in cards {
        core.feishu.reply_card(message_id, &card).await?;
    }
    Ok(())
}

/// Send the `/autoaccept` toggle card (ADR-0012, issue 05).
async fn send_autoaccept_card(
    core: &Arc<SharedCore>,
    thread_key: &ThreadKey,
    message_id: &str,
) -> crate::error::Result<()> {
    let current_on = {
        let store = core.sessions.lock().await;
        store
            .get_active(thread_key)
            .map(|e| e.auto_accept)
            .unwrap_or(false)
    };
    let card = crate::feishu::card::build_autoaccept_card(thread_key, current_on);
    core.feishu.reply_card(message_id, &card).await?;
    Ok(())
}

/// Send the `/help` navigation card (ADR-0012, issue 05): commands grouped by
/// role with a "试试" button each.
async fn send_help_card(
    core: &Arc<SharedCore>,
    thread_key: &ThreadKey,
    message_id: &str,
) -> crate::error::Result<()> {
    let card = crate::feishu::card::build_help_card(thread_key);
    core.feishu.reply_card(message_id, &card).await?;
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
    core: &Arc<SharedCore>,
    thread_key: &ThreadKey,
    keyword: &str,
    message_id: &str,
    kind: ConversationKind,
) -> crate::error::Result<()> {
    let sessions = core.cached_session_list().await?;
    let lower = keyword.to_lowercase();

    // 1. Current thread's mapped sessions first.
    let thread_ids: Vec<String> = {
        let store = core.sessions.lock().await;
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
        let mut store = core.sessions.lock().await;
        if let Some(entry) = store
            .list_thread(thread_key)
            .into_iter()
            .find(|e| e.session_id == hit.id)
            .cloned()
        {
            store.set_active(entry);
            store.persist()?;
        }
        core.feishu
            .reply_text(message_id, &format!("Switched to \"{}\".", hit.title))
            .await?;
        return Ok(());
    }
    if thread_hits.len() > 1 {
        let list = candidates_list("本会话匹配到多个，请用 `/switch <完整ID>` 指定：", &thread_hits);
        core.feishu.reply_text(message_id, &list).await?;
        return Ok(());
    }

    // 2. Global search, children excluded.
    let global_hits: Vec<&crate::opencode::SessionListInfo> = sessions
        .iter()
        .filter(|s| !s.is_child() && matches_keyword(s, &lower))
        .collect();
    if global_hits.len() == 1 {
        let hit = global_hits[0].clone();
        adopt_session(core, thread_key, &hit, message_id, kind, false).await?;
        return Ok(());
    }
    if global_hits.len() > 1 {
        let list = candidates_list("找到多个会话，请用 `/switch <完整ID>` 指定：", &global_hits);
        core.feishu.reply_text(message_id, &list).await?;
        return Ok(());
    }
    // No match: send the interactive card pre-filtered by the keyword, so the
    // user sees the empty result AND can adjust the search / start fresh.
    send_switch_card(core, thread_key, keyword, message_id).await?;
    Ok(())
}

/// `/list [keyword] [--all]` — a cached, recently-active list of every
/// session in the shared store (ADR-0008), so sessions created outside
/// Feishu become visible. Sorted by last activity (client-side), capped at
/// 15; children and archived hidden unless `--all`.
async fn handle_list(
    core: &Arc<SharedCore>,
    thread_key: &ThreadKey,
    keyword: Option<&str>,
    all: bool,
    message_id: &str,
) -> crate::error::Result<()> {
    let sessions = core.cached_session_list().await?;
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
        core.feishu
            .reply_text(message_id, "No sessions matching the filter.")
            .await?;
        return Ok(());
    }

    let (active_id, mapped_ids) = {
        let store = core.sessions.lock().await;
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
    core.feishu.reply_text(message_id, &list).await?;
    Ok(())
}

/// `/attach <id|title> [--force]` — take over an arbitrary server session
/// into the current thread (ADR-0008). Resolution: exact id → unique
/// id-prefix → unique title substring; multiple hits list candidates.
async fn handle_attach(
    core: &Arc<SharedCore>,
    thread_key: &ThreadKey,
    query: &str,
    force: bool,
    message_id: &str,
    kind: ConversationKind,
) -> crate::error::Result<()> {
    let sessions = core.cached_session_list().await?;
    let lower = query.to_lowercase();

    // Exact id.
    if let Some(s) = sessions.iter().find(|s| s.id == query) {
        return adopt_session(core, thread_key, s, message_id, kind, force).await;
    }
    // Unique id-prefix.
    let prefix: Vec<&crate::opencode::SessionListInfo> = sessions
        .iter()
        .filter(|s| s.id.to_lowercase().starts_with(&lower))
        .collect();
    if prefix.len() == 1 {
        return adopt_session(core, thread_key, prefix[0], message_id, kind, force).await;
    }
    if prefix.len() > 1 {
        let list = candidates_list("ID 前缀匹配到多个，请用完整 ID：", &prefix);
        core.feishu.reply_text(message_id, &list).await?;
        return Ok(());
    }
    // Unique title substring.
    let titles: Vec<&crate::opencode::SessionListInfo> = sessions
        .iter()
        .filter(|s| s.title.to_lowercase().contains(&lower))
        .collect();
    if titles.len() == 1 {
        return adopt_session(core, thread_key, titles[0], message_id, kind, force).await;
    }
    if titles.len() > 1 {
        let list = candidates_list("标题匹配到多个，请用完整 ID：", &titles);
        core.feishu.reply_text(message_id, &list).await?;
        return Ok(());
    }
    core.feishu
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
    core: &Arc<SharedCore>,
    thread_key: &ThreadKey,
    info: &crate::opencode::SessionListInfo,
    message_id: &str,
    kind: ConversationKind,
    force: bool,
) -> crate::error::Result<()> {
    // Idempotent: already the active session of this thread.
    {
        let store = core.sessions.lock().await;
        if let Some(e) = store.get_active(thread_key)
            && e.session_id == info.id
        {
            core.feishu
                .reply_text(message_id, &format!("Already active: \"{}\".", info.title))
                .await?;
            return Ok(());
        }
    }
    // Mapped to another thread: reject unless --force.
    let owner = {
        let store = core.sessions.lock().await;
        store.thread_for_session(&info.id)
    };
    if let Some(owner_key) = owner
        && owner_key != *thread_key
    {
        if !force {
            let chat_name = core
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
            core.feishu
                    .reply_text(
                        message_id,
                        &format!(
                            "⚠️ 会话 `{}`（目录 `{}`）已被其他聊天占用：\n{}\n（{}，chat `{}`）\n\n可先请对方 `/switch forget` 解除，或使用 `/switch {} --force` 强行接管。",
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
        let mut store = core.sessions.lock().await;
        store.remove(&info.id);
        store.persist()?;
    }

    let anchor = if kind == ConversationKind::Topic {
        match core
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
        model: None,
        auto_accept: false,
        topic_anchor: anchor,
    };
    {
        let mut store = core.sessions.lock().await;
        store.set_active(entry);
        store.persist()?;
    }
    core.invalidate_session_list_cache().await;
    // In a topic the confirmation was already sent inside it (the
    // `reply_in_thread` above doubles as the fallback-card anchor); don't
    // reply twice.
    if kind != ConversationKind::Topic {
        core.feishu
            .reply_text(
                message_id,
                &format!("已接管会话 `{}`（目录 `{}`）。", info.title, info.directory),
            )
            .await?;
    }
    Ok(())
}

/// The last 7 characters of a session id (display suffix).
pub(crate) fn id_tail(id: &str) -> String {
    id.strip_prefix("ses_").unwrap_or(id).chars().take(7).collect()
}

/// Normalize a user-supplied directory for `/dir` / `/topic` into an absolute
/// path the OpenCode server can route by: expand a leading `~`, resolve
/// relative paths against the working directory, and canonicalize (`..`,
/// symlinks) when the path exists. OpenCode sessions are keyed by their
/// directory and the server fails on a session created with a `~`-style or
/// relative path, so cola must hand it a real absolute directory.
fn normalize_directory(input: &str) -> std::path::PathBuf {
    let home = dirs::home_dir().unwrap_or_default();
    let expanded = if input == "~" {
        home.clone()
    } else if let Some(rest) = input.strip_prefix("~/") {
        home.join(rest)
    } else {
        std::path::PathBuf::from(input)
    };
    let expanded = if expanded.is_absolute() {
        expanded
    } else {
        std::env::current_dir().unwrap_or_default().join(expanded)
    };
    // Canonicalize (resolving `..` and symlinks) when the directory exists;
    // otherwise keep the normalized absolute form — the caller checks
    // `is_dir()` and reports it as missing.
    std::fs::canonicalize(&expanded).unwrap_or(expanded)
}

/// Normalize + validate a user-supplied directory for `/dir`/`/topic`. On a
/// bad path replies a clear error and returns `None`; otherwise returns the
/// normalized absolute directory string.
async fn resolve_directory_or_reply(
    feishu: &dyn feishu::Platform,
    message_id: &str,
    input: &str,
    hint: &str,
) -> crate::error::Result<Option<String>> {
    let dir = normalize_directory(input);
    if !dir.is_dir() {
        feishu
            .reply_text(
                message_id,
                &format!(
                    "⚠️ 目录不存在：`{}`（解析为 `{}`）。\n请确认路径正确后再 {}",
                    input,
                    dir.display(),
                    hint
                ),
            )
            .await?;
        return Ok(None);
    }
    Ok(Some(dir.to_string_lossy().to_string()))
}

/// Case-insensitive keyword match on a session's title, directory or id
/// (shared by `/list`, `/switch`, `/attach` and the `/switch` card).
pub(crate) fn matches_keyword(s: &crate::opencode::SessionListInfo, lower: &str) -> bool {
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
pub(crate) fn title_or_id_tail(s: &crate::opencode::SessionListInfo) -> String {
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
    }

    #[test]
    fn parse_agent_model_no_arg_is_card() {
        assert_eq!(parse_command("/model"), Some(Command::ModelCard));
        assert_eq!(parse_command("/agent"), Some(Command::AgentCard));
    }

    #[test]
    fn parse_switch() {
        assert_eq!(
            parse_command("/switch backend"),
            Some(Command::Switch(SwitchAction::Match("backend".into())))
        );
    }

    #[test]
    fn parse_switch_no_arg_is_card() {
        assert_eq!(
            parse_command("/switch"),
            Some(Command::Switch(SwitchAction::Card))
        );
    }

    #[test]
    fn parse_switch_list() {
        assert_eq!(
            parse_command("/switch list"),
            Some(Command::Switch(SwitchAction::List {
                keyword: None,
                all: false
            }))
        );
        assert_eq!(
            parse_command(" /switch list "),
            Some(Command::Switch(SwitchAction::List {
                keyword: None,
                all: false
            }))
        );
    }

    #[test]
    fn parse_switch_list_keyword_and_all() {
        assert_eq!(
            parse_command("/switch list cola"),
            Some(Command::Switch(SwitchAction::List {
                keyword: Some("cola".into()),
                all: false
            }))
        );
        assert_eq!(
            parse_command("/switch list --all"),
            Some(Command::Switch(SwitchAction::List {
                keyword: None,
                all: true
            }))
        );
        assert_eq!(
            parse_command("/switch list cola --all"),
            Some(Command::Switch(SwitchAction::List {
                keyword: Some("cola".into()),
                all: true
            }))
        );
        assert_eq!(
            parse_command("/switch list multi word"),
            Some(Command::Switch(SwitchAction::List {
                keyword: Some("multi word".into()),
                all: false
            }))
        );
    }

    #[test]
    fn parse_switch_attach() {
        assert_eq!(
            parse_command("/switch ses_abc123 --force"),
            Some(Command::Switch(SwitchAction::Attach {
                query: "ses_abc123".into(),
                force: true
            }))
        );
    }

    #[test]
    fn parse_switch_forget() {
        assert_eq!(
            parse_command("/switch forget"),
            Some(Command::Switch(SwitchAction::Forget))
        );
    }

    #[test]
    fn parse_restart() {
        assert_eq!(parse_command("/restart"), Some(Command::Restart));
        assert_eq!(parse_command("/restart now"), Some(Command::Restart));
        assert_eq!(parse_command("/update"), Some(Command::Update));
        assert_eq!(parse_command("/update now"), Some(Command::Update));
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
            parse_command("  /switch list  "),
            Some(Command::Switch(SwitchAction::List {
                keyword: None,
                all: false
            }))
        );
    }

    #[test]
    fn case_insensitive_command() {
        assert_eq!(parse_command("/STOP"), Some(Command::Stop));
        assert_eq!(
            parse_command("/Switch List"),
            Some(Command::Switch(SwitchAction::List {
                keyword: None,
                all: false
            }))
        );
    }

    #[test]
    fn normalize_directory_expands_tilde() {
        let home = dirs::home_dir().unwrap();
        let sub = normalize_directory("~/.cola");
        assert!(
            sub.starts_with(&home),
            "~/.cola must expand under home: {}",
            sub.display()
        );
        assert!(sub.ends_with(".cola"));
        // Bare `~` resolves to home itcore.
        let home_only = normalize_directory("~");
        assert_eq!(home_only, home);
    }

    #[test]
    fn normalize_directory_makes_relative_absolute() {
        let cwd = std::env::current_dir().unwrap();
        let abs = normalize_directory("some/relative/dir");
        assert!(
            abs.starts_with(&cwd),
            "relative path must resolve against cwd: {}",
            abs.display()
        );
        assert!(abs.ends_with("some/relative/dir"));
    }

    #[test]
    fn normalize_directory_keeps_existing_absolute_path() {
        let cwd = std::env::current_dir().unwrap();
        let abs = normalize_directory(&cwd.to_string_lossy());
        assert_eq!(abs, cwd, "an existing absolute path canonicalizes to itself");
        // A nonexistent absolute path is preserved (caller reports it missing).
        let missing = normalize_directory("/nonexistent/dir/xyz");
        assert_eq!(missing, std::path::PathBuf::from("/nonexistent/dir/xyz"));
    }
}
