/// Parsed slash command from user message text.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// Change project directory for the current session
    Dir(String),
    /// Switch to a named session in the current thread
    Switch(String),
    /// List all sessions in the current thread
    List,
    /// Create a fresh session, optionally named
    New(Option<String>),
    /// Rename the current session
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
        "/list" => Some(Command::List),
        "/new" => Some(Command::New(arg.map(|s| s.to_string()))),
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
`/switch <name>` · Switch to named session
`/list` · List sessions in this thread
`/new [name]` · Create a new session
`/name <name>` · Rename current session
`/stop` · Interrupt execution
`/compact` · Compact context
`/agent <name>` · Switch agent
`/model <p/m>` · Switch model
`/autoaccept` · Show auto-approve status; `/autoaccept on|off` switches
`/restart` · Restart cola (keeps startup args + log redirect)
`/restart-opencode` · Restart the OpenCode server (only when cola started it)
`/help <command>` · Show help for one command (e.g. `/help model`)
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
            "/switch <name>\nSwitch to an existing named session in this thread. Run `/list` to see names.\nExample: `/switch backend`"
        }
        "list" => {
            "/list\nList all sessions in this thread: name, id and directory, with the active one marked."
        }
        "new" => {
            "/new [name]\nCreate a fresh session, optionally named. Without a name a generated one is used.\nExample: `/new api-refactor`"
        }
        "name" => "/name <name>\nRename the current session.\nExample: `/name frontend`",
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
        assert_eq!(parse_command("/list"), Some(Command::List));
        assert_eq!(parse_command(" /list "), Some(Command::List));
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
        assert_eq!(parse_command("  /list  "), Some(Command::List));
    }

    #[test]
    fn case_insensitive_command() {
        assert_eq!(parse_command("/STOP"), Some(Command::Stop));
        assert_eq!(parse_command("/List"), Some(Command::List));
    }
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
                        .reply_text(message_id, &format!("Switched to \"{}\"", name))
                        .await?;
                } else {
                    self.feishu
                        .reply_text(message_id, &format!("No session matching \"{}\"", name))
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
}
