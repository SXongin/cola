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
    /// Show available commands
    Help,
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
        "/dir" => Some(Command::Dir(arg?.to_string())),
        "/switch" => Some(Command::Switch(arg?.to_string())),
        "/list" => Some(Command::List),
        "/new" => Some(Command::New(arg.map(|s| s.to_string()))),
        "/name" => Some(Command::Name(arg?.to_string())),
        "/stop" => Some(Command::Stop),
        "/compact" => Some(Command::Compact),
        "/agent" => Some(Command::Agent(arg?.to_string())),
        "/model" => Some(Command::Model(arg?.to_string())),
        "/help" => Some(Command::Help),
        // `/init`, `/review`, or any unknown /command — forward to OpenCode
        _ => Some(Command::Forward(trimmed.to_string())),
    }
}

/// Help text shown for the `/help` command.
pub fn help_text() -> String {
    "\
**cola commands**
`/dir <path>` · Change project directory
`/switch <name>` · Switch to named session
`/list` · List sessions in this thread
`/new [name]` · Create a new session
`/name <name>` · Rename current session
`/stop` · Interrupt execution
`/compact` · Compact context
`/agent <name>` · Switch agent
`/model <p/m>` · Switch model
`/help` · Show this help
    "
    .to_string()
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
    fn parse_dir_missing_arg_returns_none() {
        assert_eq!(parse_command("/dir"), None);
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
        assert_eq!(parse_command("/help"), Some(Command::Help));
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
