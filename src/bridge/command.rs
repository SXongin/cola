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
    /// Toggle auto-accept of permission requests for the current session.
    AutoAccept(bool),
    /// Restart cola itself, preserving startup args and the log redirect.
    Restart,
    /// Show available commands, or help for one command (`/help <cmd>`).
    Help(Option<String>),
    /// Forward unrecognized slash command to OpenCode as prompt text
    Forward(String),
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
        // `/autoaccept` toggles; `/autoaccept on|off` sets explicitly.
        "/autoaccept" => match arg {
            Some("on") | Some("true") | Some("1") => Some(Command::AutoAccept(true)),
            Some("off") | Some("false") | Some("0") => Some(Command::AutoAccept(false)),
            Some(other) => match other.parse::<bool>() {
                Ok(b) => Some(Command::AutoAccept(b)),
                Err(_) => None,
            },
            None => Some(Command::AutoAccept(true)),
        },
        "/restart" => Some(Command::Restart),
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
`/autoaccept [on|off]` · Auto-allow permissions for this session
`/restart` · Restart cola (keeps startup args + log redirect)
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
            "/autoaccept [on|off]\nToggle auto-allowing permission requests for this session (no permission cards).\nDefault (no arg): on. Example: `/autoaccept on`"
        }
        "restart" => {
            "/restart\nRestart cola itself, keeping startup args and the log redirect. cola announces in this chat when it's back."
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
