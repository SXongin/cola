/// The pending line for an explicit `/think --reset`: the adapter carries no
/// model or effort to name, only the clear the next turn will apply.
pub(crate) const PENDING_CLEAR_LABEL: &str = "清除思考等级";

/// The display value for a live thread's next-turn pending selection, shared
/// by `/status` and the model card so they cannot drift. A pending model is
/// its adapter reference; an effort-only pending with no thread model fact
/// shows the level itself; a reset-only marker shows the clear. `None` when
/// nothing is pending.
pub(crate) async fn pending_label(
    backend: &std::sync::Arc<dyn crate::backend::Backend>,
    session_id: &str,
) -> Option<String> {
    if let Some(model) = backend.pending_model(session_id).await {
        return Some(model);
    }
    if !backend.has_pending_selection(session_id).await {
        return None;
    }
    match backend.pending_variant(session_id).await {
        Some(level) => Some(format!("思考等级 {level}")),
        None => Some(PENDING_CLEAR_LABEL.to_string()),
    }
}

/// Parsed slash command from user message text.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// Change project directory for the current session
    Dir(String),
    /// `/dir` (no args) — interactive Recent Directories card.
    DirCard,
    /// Session management (`/switch ...`): match, forget, adopt, or the
    /// interactive card. Absorbed the old `/attach` and `/forget`.
    Switch(SwitchAction),
    /// Child-session family (`/sub ...`, spec #344): the Active Session's
    /// direct child sessions — the read-only list, or the explicit takeover of
    /// one of them into this chat.
    Sub(SubAction),
    /// Create a fresh session, optionally named
    New(Option<String>),
    /// `/new --worktree [--name <title>] [--branch <branch>] [--base <ref>]`
    /// — the same checkout intent as `TopicWorktree`, in this conversation
    /// (ADR-0095).
    NewWorktree {
        name: Option<String>,
        branch: Option<String>,
        base: Option<String>,
    },
    /// Create a real Feishu topic backed by a fresh session. `directory` is
    /// `None` for the bare `/topic` form, which inherits the conversation's
    /// current project (the active session's directory, ADR-0012) instead of an
    /// explicit path.
    Topic {
        directory: Option<String>,
        name: Option<String>,
    },
    /// `/topic --worktree [--name <title>] [--branch <branch>]
    /// [--base <ref>]` — open a topic whose first message creates a linked
    /// checkout and the Session inside it (ADR-0095). Nothing is created at
    /// command time.
    TopicWorktree {
        name: Option<String>,
        branch: Option<String>,
        base: Option<String>,
    },
    /// Create a real Feishu topic whose backing session is an EXISTING server
    /// session (ADR-0016): resolve by keyword, adopt, and map to the new topic.
    /// `force` steals a session mapped to another thread.
    TopicAdopt { keyword: String, force: bool },
    /// `/topic --adopt` (no keyword) — the interactive session-picker card,
    /// whose per-row button creates a topic around the chosen session.
    TopicAdoptCard,
    /// Rename the current session (PATCHes the server title)
    Name(String),
    /// Interrupt the current session execution
    Stop,
    /// Compact the current session context
    Compact,
    /// Pull the live card back to the newest position: split the Card Chain at
    /// this command message (ADR-0043, 2026-09-22 amendment) — the explicit
    /// exception to "commands never split the chain".
    Card,
    /// Switch agent in the current session: an available agent name, or
    /// `--reset` to clear the per-session override (back to the server's
    /// default agent).
    Agent(String),
    /// `/agent` (no args) — interactive agent-picker card.
    AgentCard,
    /// Switch model in the current session
    Model(String),
    /// `/model` (no args) — interactive model-picker card.
    ModelCard,
    /// Set the thinking level (model-declared variant) in the current session:
    /// a concrete variant name, or `--reset` to clear it.
    Think(String),
    /// `/think` (no args) — interactive variant-picker card.
    ThinkCard,
    /// Auto-accept of permission requests for the current session.
    AutoAccept(AutoAcceptAction),
    /// Restart cola itself, preserving startup args and the log redirect.
    Restart,
    /// Restart the OpenCode server — but only when cola started it. A server
    /// another tool launched is never touched.
    RestartOpenCode,
    /// Update cola (ADR-0015, ADR-0030): check the install channel. A GitHub
    /// Releases install downloads, verifies, replaces the running binary, and
    /// restarts; a cargo-tracked install is told which cargo command updates it.
    Update,
    /// Show the running cola version and build provenance (release or dev
    /// build) — ADR-0027. Never talks to the network.
    Version,
    /// Show this conversation's current model/effort, any next-turn pending,
    /// and a short session summary (spec #922). A text reply, no card.
    Status,
    /// Show available commands, or help for one command (`/help <cmd>`).
    Help(Option<String>),
    /// Load one or more OpenCode skills into the prompt (spec #652, tickets
    /// #654/#656; the `#<id>` syntax, spec #662). The whole original message
    /// text is the prompt, tokens and all — it mirrors the composer's
    /// `@skill-id`. The coordinator routes this into the prompt pipeline itself
    /// — the same path as [`Command::Forward`] — because the prompt pipeline,
    /// not the command dispatcher, owns submission. See [`SkillInvocation`] for
    /// the two forms.
    Skill(SkillInvocation),
    /// Forward unrecognized slash command to OpenCode as prompt text
    Forward(String),
    /// `/cat <path>` — open one **Host** file as a **File Viewer** card, or a
    /// directory as the **Directory Browser** (ADR-0083 §2). `None` is the bare
    /// `/cat`, which answers usage instead of becoming a prompt. The path
    /// resolves on the **Path Scope** (relative to the Session's directory,
    /// `~` expanded, absolute passed through) and is advisory, not a boundary.
    Cat(Option<String>),
    /// `/diff [--base <ref>]` — render the repository review (committed +
    /// uncommitted + untracked changes against a base ref) as one HTML File
    /// Message (spec #803). `None` means no `--base`: the configured
    /// `[review] base`, else the producer's `origin/main` → `main` chain.
    Diff(Option<String>),
    /// Any other `/diff` argument — an unknown flag, extra words, or a bare
    /// `--base` with no value. Answered with one usage line and no git call.
    DiffUsage,
    /// `/run [--timeout <n>[smh]] [--] <cmd>` — one **Terminal Command**
    /// (ADR-0086): the command text and its cap. `None` is the bare `/run` or a
    /// refused flag form, both answered with usage and nothing spawned.
    Run(Option<RunSpec>),
    /// `/worktree` — report the pending checkout intent (ADR-0095). Outside a
    /// pending it answers usage; the read-only inventory card is retired in
    /// favour of `/run git worktree list`.
    WorktreeStatus,
    /// `/worktree --branch <branch> [--base <ref>]` — adjust the pending
    /// intent before the first message creates it (ADR-0095).
    WorktreeAdjust {
        branch: Option<String>,
        base: Option<String>,
    },
    /// A malformed `/worktree` form (including the retired `new` and `list`)
    /// — answered with usage and no git call.
    WorktreeUsage,
}

/// One parsed `/run` invocation (ADR-0086 §2, §6): the command text after flag
/// parsing (everything after the flags, or after `--`) and that run's cap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunSpec {
    /// The exact command text handed to `sh -c`; flags were parsed only before
    /// it, so a command may carry `--timeout`-looking words of its own.
    pub command: String,
    /// The run's cap; the default when `--timeout` names none.
    pub timeout: std::time::Duration,
}

/// The default cap for a `/run` (`--timeout` names its own): ten minutes.
pub const RUN_DEFAULT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// The hard maximum for `--timeout`: twenty-four hours.
pub const RUN_MAX_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

/// One skill-invocation request (spec #662): the ids a message named, and which
/// form named them.
#[derive(Debug, Clone, PartialEq)]
pub struct SkillInvocation {
    /// The ids the message named, in first-occurrence order. `/skill <id>`
    /// contributes the token after each `/skill`; a plain message contributes
    /// each `#<id>` whose id starts with an ASCII letter.
    pub ids: Vec<String>,
    /// Whether a dispatch that resolves NO id still owes a picker: `/skill` —
    /// the list-entry command — answers the skill picker (a bare `/skill`, or
    /// ids that all fail to resolve). `false` for a `#<id>` message, whose
    /// unresolvable ids are ordinary prose: the message submits as a plain
    /// prompt, no card.
    pub picker_on_empty: bool,
}

impl Command {
    /// The single source of truth for the "commands restricted inside a topic"
    /// rule (ADR-0007, ADR-0023): `handle_command` calls this at its top.
    ///
    /// - `/topic` and its adopt forms would nest a topic inside a topic: they
    ///   are rejected in ANY topic, whether or not it already has a session.
    /// - Session selection/creation (`/dir`, the `/dir` card, the selecting
    ///   `/switch` forms, `/sub attach`, `/new`) is allowed while the topic is
    ///   unbound — the chosen session becomes the topic's one session — and
    ///   rejected once the topic owns a session.
    /// - `/switch forget` is NOT a selection: it only drops the mapping (the
    ///   server session stays), which is the one thing a bound topic needs to
    ///   do for itself — so it passes bound and unbound alike (#997). The
    ///   main conversation cannot do it: its `forget` unmaps its own key.
    /// - Every other command passes.
    pub(crate) fn topic_rejection(&self, has_session: bool) -> Option<&'static str> {
        // The one `/switch` form that is NOT a selection: `forget` drops the
        // mapping and leaves the server session alone, so a bound topic may
        // always run it (#997). Stated ahead of the table so the exception is
        // visible, and so a future `SwitchAction` variant is refused by
        // default rather than silently allowed.
        if matches!(self, Command::Switch(SwitchAction::Forget)) {
            return None;
        }
        match self {
            Command::Topic { .. } | Command::TopicWorktree { .. } => Some(TOPIC_NEST_REJECTION),
            Command::TopicAdopt { .. } | Command::TopicAdoptCard => Some(TOPIC_ADOPT_NEST_REJECTION),
            Command::Dir(_)
            | Command::DirCard
            | Command::Switch(_)
            | Command::Sub(SubAction::Attach { .. })
            | Command::New(_)
            | Command::NewWorktree { .. }
                if has_session =>
            {
                Some(TOPIC_SELECTION_REJECTION)
            }
            _ => None,
        }
    }
}

/// The rejection for the session-selection forms — `Dir`/`DirCard`, `Switch`'s
/// `Card`/`Match`/`Attach`, `Sub::Attach` and `New` — inside a topic that
/// already owns a session (ADR-0007): a bound topic is a conversation of its
/// own, so sessions are chosen from the main conversation. `/switch forget` is
/// deliberately not one of them (#997: it unbinds rather than selects).
pub(crate) const TOPIC_SELECTION_REJECTION: &str = "⚠️ 话题已绑定会话，请回主对话操作。";

/// The rejection for `/topic` inside any topic (ADR-0006, ADR-0023): a topic
/// never nests inside another topic, bound or not.
pub(crate) const TOPIC_NEST_REJECTION: &str =
    "⚠️ /topic 只能从会话顶层使用，不能在话题里再开话题。请在主会话里发 /topic <目录>。";

/// The rejection for `/topic --adopt` inside any topic — one text shared by
/// the keyword form and the no-arg picker card (ADR-0016, ADR-0023).
pub(crate) const TOPIC_ADOPT_NEST_REJECTION: &str =
    "⚠️ /topic --adopt 只能从会话顶层使用，不能在话题里开话题。请在主会话里发 /topic --adopt <会话>。";

/// `/card` with no Session mapped to this conversation: there is nothing to
/// read, so one text line says so. The 「当前没有正在运行的实时卡片」 notice is
/// retired (ADR-0100) — an existing Session always answers with a card, either
/// the render-owned one pulled down or a one-shot read.
const NO_SESSION_TO_READ: &str = "当前没有可读取的会话。";

/// `/stop` with no live card to finalize (an idle session, or a card already
/// at a terminal): nothing will render the stop, so the command itself says
/// what happened. With a render-owned card the reply is suppressed — its
/// settle loop stamps 「⏹ 已停止」 in place (#394), and that card IS the
/// acknowledgement.
pub(crate) const NO_RUNNING_TASK: &str = "当前没有正在执行的任务。";

/// `/stop` on a card that yielded 「⏳ 等待后台任务」 (ADR-0059/0060): the Turn
/// has no render loop to stamp the stop promptly, and the card only settles at
/// the quiet true end — once the last Background Task retires — so the command
/// acks and names the ending that will land then.
pub(crate) const STOPPED_WAITING: &str = "⏹ 已停止等待；后台任务结束后卡片会标记为已停止。";

/// `/compact` on a mapped session: the submit acknowledgement, and the only
/// thing the command replies — no result is rendered here. The compaction itself
/// is surfaced by the card that owns the chain, in its own server-time place
/// (ADR-0082), so the ack says where to look instead of promising a chat reply.
const COMPACTING: &str = "已提交压缩，完成后会显示在卡片上。";

/// What `/switch` should do (ADR-0012). The text-direct forms all share the
/// session store; the no-arg form pops the interactive card.
#[derive(Debug, Clone, PartialEq)]
pub enum SwitchAction {
    /// `/switch` (no args) — the interactive session card (issue 04).
    Card,
    /// `/switch <keyword>` — matching rules: thread's sessions first, then a
    /// unique global match is adopted (ADR-0008).
    Match(String),
    /// `/switch forget` — the old `/forget`.
    Forget,
    /// `/switch <id|title> [--force]` — the old `/attach`.
    Attach { query: String, force: bool },
}

/// What `/sub` should do (spec #344): the read-only view of the Active
/// Session's direct children and the explicit takeover of one of them.
#[derive(Debug, Clone, PartialEq)]
pub enum SubAction {
    /// `/sub`, `/sub list` or `/sub list <keyword>` — open the child-session
    /// card, narrowed by the keyword when one was given.
    List(String),
    /// `/sub attach <id|id-prefix|title> [--force]` — take over one of the
    /// Active Session's DIRECT children into the current chat. The query
    /// resolves like `/switch` (exact id → unique id-prefix → title substring)
    /// but scoped to the direct children; `force` steals a child mapped to
    /// another chat.
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

/// Parse a command from message text. A `/`-leading message is a slash command;
/// a message that leads with anything else is a plain prompt unless it carries
/// `#<id>` skill tokens (spec #662), in which case it parses as
/// [`Command::Skill`] with [`SkillInvocation::picker_on_empty`] `false`. Returns
/// `None` for a plain message with no skill tokens.
pub fn parse_command(text: &str) -> Option<Command> {
    let trimmed = text.trim();
    if !trimmed.starts_with('/') {
        // A plain message can still load skills via `#<id>` tokens (spec #662).
        // The `#` must open a token (start of text, or after a non-id character)
        // and the id after it must start with an ASCII letter, so prose like
        // `#644` (an issue reference), `# 标题` (a markdown heading) or
        // `C#caveman` never enters the skill path — those messages stay plain
        // prompts and cost no list read.
        let ids = parse_hash_skill_ids(trimmed);
        if ids.is_empty() {
            return None;
        }
        return Some(Command::Skill(SkillInvocation {
            ids,
            picker_on_empty: false,
        }));
    }

    let parts: Vec<&str> = trimmed.splitn(2, ' ').collect();
    let cmd = parts[0].to_lowercase();
    let arg = parts.get(1).map(|s| s.trim()).filter(|s| !s.is_empty());

    match cmd.as_str() {
        // Commands that need an arg show their own help when the arg is missing,
        // instead of silently becoming an AI prompt.
        "/dir" => match arg {
            Some(p) => Some(Command::Dir(p.to_string())),
            None => Some(Command::DirCard),
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
        // `/sub [list [keyword]]` / `/sub attach <query> [--force]` — the
        // Active Session's direct children, read-only or taken over (spec
        // #344). The grammar is exactly the sanctioned forms: no-arg, `list`,
        // `list <keyword>`, `attach <query> [--force]`. Any other first word (a
        // bare keyword, or an `attach` with no query) is not a form — it gets
        // the `/sub` help topic instead of being read as a keyword.
        "/sub" => match arg {
            None => Some(Command::Sub(SubAction::List(String::new()))),
            Some(a) => {
                let mut words = a.split_whitespace();
                match words.next() {
                    Some("list") => {
                        let keyword = words.collect::<Vec<_>>().join(" ");
                        Some(Command::Sub(SubAction::List(keyword)))
                    }
                    Some("attach") => {
                        let mut query: Vec<&str> = Vec::new();
                        let mut force = false;
                        for w in words {
                            if w == "--force" {
                                force = true;
                            } else {
                                query.push(w);
                            }
                        }
                        if query.is_empty() {
                            Some(Command::Help(Some("sub".into())))
                        } else {
                            Some(Command::Sub(SubAction::Attach {
                                query: query.join(" "),
                                force,
                            }))
                        }
                    }
                    _ => Some(Command::Help(Some("sub".into()))),
                }
            }
        },
        "/new" => match arg {
            Some(a) if a.split_whitespace().next() == Some("--worktree") => Some(parse_new_worktree(a)),
            // ADR-0095 decision 8: `--base` without `--worktree` is usage, not
            // a title. Any other leading flag is equally unaccounted for.
            Some(a) if a.trim_start().starts_with("--") => Some(Command::WorktreeUsage),
            other => Some(Command::New(other.map(|s| s.to_string()))),
        },
        // `/cat <path>` opens one host file as a card, a directory as the
        // browse card, and a bare `/cat` answer usage (ADR-0083 §2). The whole
        // remainder of the line is the path, so a path with spaces survives.
        "/cat" => Some(Command::Cat(arg.map(|s| s.to_string()))),
        // `/diff [--base <ref>]` — the repository review (spec #803). Only
        // these two forms are sanctioned; any other argument (an unknown flag,
        // extra words, or a bare `--base` with no value) is answered with one
        // usage line before any git call.
        "/diff" => {
            let words: Vec<&str> = arg.map(|a| a.split_whitespace().collect()).unwrap_or_default();
            match words.as_slice() {
                [] => Some(Command::Diff(None)),
                ["--base", base] if !base.starts_with('-') => Some(Command::Diff(Some((*base).to_string()))),
                _ => Some(Command::DiffUsage),
            }
        }
        // `/worktree` — the pending checkout intent (ADR-0095). No argument
        // reports it; `--branch`/`--base` adjust it before the first message.
        // The old card (`list`) and `new` forms are retired.
        "/worktree" => Some(match arg {
            None => Command::WorktreeStatus,
            Some(a) => parse_worktree_arg(a),
        }),
        // `/run [--timeout <n>[smh]] [--] <cmd>` — one Terminal Command
        // (ADR-0086 §2, §6). Flags are parsed only before the command, and `--`
        // ends flag parsing so a command may start with `-`; a bare `/run` or a
        // refused flag form answers usage and runs nothing.
        "/run" => Some(Command::Run(parse_run_arg(arg))),
        "/topic" => match arg {
            Some(a) => {
                // `/topic --worktree [--name …] [--branch …] [--base …]` — a
                // topic whose first message creates a checkout (ADR-0095).
                if a.split_whitespace().next() == Some("--worktree") {
                    return Some(parse_topic_worktree(a));
                }
                // `/topic --adopt <keyword> [--force]` — adopt an existing
                // session into a new topic; `/topic --adopt` pops the card.
                if a.split_whitespace().next() == Some("--adopt") {
                    let mut words: Vec<&str> = Vec::new();
                    let mut force = false;
                    for w in a.split_whitespace().skip(1) {
                        if w == "--worktree" {
                            // ADR-0095 decision 8: adopting an existing
                            // session never creates a checkout.
                            return Some(Command::WorktreeUsage);
                        }
                        if w == "--force" {
                            force = true;
                        } else {
                            words.push(w);
                        }
                    }
                    let keyword = words.join(" ");
                    if keyword.is_empty() {
                        Some(Command::TopicAdoptCard)
                    } else {
                        Some(Command::TopicAdopt { keyword, force })
                    }
                } else if a.trim_start().starts_with("--") {
                    // `--base` without `--worktree`, or any other unknown flag.
                    Some(Command::WorktreeUsage)
                } else {
                    // `/topic <dir>` or `/topic <dir> <name>`
                    let mut it = a.splitn(2, ' ');
                    let dir = it.next().unwrap_or("").trim();
                    let name = it.next().map(|s| s.trim()).filter(|s| !s.is_empty());
                    if dir.is_empty() {
                        Some(Command::Help(Some("topic".into())))
                    } else {
                        Some(Command::Topic {
                            directory: Some(dir.to_string()),
                            name: name.map(|s| s.to_string()),
                        })
                    }
                }
            }
            // `/topic` (no args) — a topic in the conversation's current
            // project (the active session's directory), like `/new`.
            None => Some(Command::Topic {
                directory: None,
                name: None,
            }),
        },
        "/name" => match arg {
            Some(p) => Some(Command::Name(p.to_string())),
            None => Some(Command::Help(Some("name".into()))),
        },
        "/stop" => Some(Command::Stop),
        "/compact" => Some(Command::Compact),
        "/card" => Some(Command::Card),
        "/agent" => match arg {
            Some(p) => Some(Command::Agent(p.to_string())),
            None => Some(Command::AgentCard),
        },
        "/model" => match arg {
            Some(p) => Some(Command::Model(p.to_string())),
            None => Some(Command::ModelCard),
        },
        // `/think` sets/clears the thinking level (a model-declared variant);
        // `/think` (no args) pops the variant-picker card. `default`/`off`/
        // `reset` all clear the override.
        "/think" => match arg {
            None => Some(Command::ThinkCard),
            Some(p) => Some(Command::Think(p.to_string())),
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
        "/version" => Some(Command::Version),
        "/status" => Some(Command::Status),
        "/help" => Some(Command::Help(arg.map(|s| s.to_lowercase()))),
        // `/skill <id> [/skill <id> …] [text]` — load the named skills into the
        // prompt (spec #652). Each `/skill` token takes the token that follows
        // it as an id; every other token stays in the message text, which is
        // submitted verbatim. A bare `/skill` carries no id and opens the
        // picker. (The `#<id>` form is parsed above, for messages that do not
        // lead with `/`; spec #662.)
        "/skill" => Some(Command::Skill(SkillInvocation {
            ids: parse_skill_ids(trimmed),
            picker_on_empty: true,
        })),
        // `/init`, `/review`, or any unknown /command — forward to OpenCode
        _ => Some(Command::Forward(trimmed.to_string())),
    }
}

/// Consume one flag value, refusing a leading `-` (a missing value is
/// `None`). Shared by the creation and adjustment parsers so their syntax
/// cannot drift.
fn flag_value<'a>(words: &mut impl Iterator<Item = &'a str>) -> Option<&'a str> {
    words.next().filter(|value| !value.starts_with('-'))
}

/// Parse the `--name` / `--branch` / `--base` flags shared by the two
/// worktree-creating forms (ADR-0095). `None` for an unknown flag, a bare
/// flag, a duplicate, or a positional word — all answered with usage. `--name`
/// takes words up to the next flag; `--branch` and `--base` take one word.
fn parse_worktree_flags(arg: &str) -> Option<(Option<String>, Option<String>, Option<String>)> {
    let mut name = None;
    let mut branch = None;
    let mut base = None;
    // `--name` takes the remaining words up to the next recognised flag, so an
    // English title keeps its spaces (`--name Fix Login`, like `/new Fix
    // Login`); the two machine-facing values stay one word each.
    let mut words = arg.split_whitespace().peekable();
    while let Some(word) = words.next() {
        match word {
            "--name" => {
                if name.is_some() {
                    return None;
                }
                let mut parts = Vec::new();
                while let Some(next) = words.peek() {
                    if matches!(*next, "--name" | "--branch" | "--base") {
                        break;
                    }
                    parts.push(*next);
                    words.next();
                }
                if parts.is_empty() {
                    return None;
                }
                name = Some(parts.join(" "));
            }
            "--branch" => {
                if branch.is_some() {
                    return None;
                }
                branch = Some(flag_value(&mut words)?.to_string());
            }
            "--base" => {
                if base.is_some() {
                    return None;
                }
                base = Some(flag_value(&mut words)?.to_string());
            }
            _ => return None,
        }
    }
    Some((name, branch, base))
}

/// `/topic --worktree …` (ADR-0095): the directory is the conversation's
/// current project, exactly like the bare `/topic`; the flags only describe
/// the checkout the first message creates.
fn parse_topic_worktree(arg: &str) -> Command {
    match parse_worktree_flags(arg.strip_prefix("--worktree").unwrap_or(arg).trim_start()) {
        Some((name, branch, base)) => Command::TopicWorktree { name, branch, base },
        None => Command::WorktreeUsage,
    }
}

/// `/new --worktree …` (ADR-0095): the same flags, declared in the current
/// conversation instead of a new topic.
fn parse_new_worktree(arg: &str) -> Command {
    match parse_worktree_flags(arg.strip_prefix("--worktree").unwrap_or(arg).trim_start()) {
        Some((name, branch, base)) => Command::NewWorktree { name, branch, base },
        None => Command::WorktreeUsage,
    }
}

/// Parse `/worktree`'s argument (ADR-0095). The grammar is bare `/worktree`
/// (the pending status) and `--branch <branch>` / `--base <ref>` (adjust the
/// pending intent), in any order, at least one flag. Everything else — the
/// retired `new`/`list`, a bare or duplicate flag, a positional word — is
/// usage, with no git call.
fn parse_worktree_arg(arg: &str) -> Command {
    let mut branch = None;
    let mut base = None;
    let mut words = arg.split_whitespace();
    while let Some(word) = words.next() {
        let slot = match word {
            "--branch" => &mut branch,
            "--base" => &mut base,
            _ => return Command::WorktreeUsage,
        };
        if slot.is_some() {
            return Command::WorktreeUsage;
        }
        let Some(value) = flag_value(&mut words) else {
            return Command::WorktreeUsage;
        };
        *slot = Some(value.to_string());
    }
    if branch.is_none() && base.is_none() {
        Command::WorktreeUsage
    } else {
        Command::WorktreeAdjust { branch, base }
    }
}

/// Parse `/run`'s argument (ADR-0086 §6): `--timeout <n>[smh]` flags, then the
/// command — or, after `--`, the rest as the command verbatim, so a command may
/// begin with `-`. `None` means usage: a bare `/run`, a bare or malformed
/// `--timeout`, a value past [`RUN_MAX_TIMEOUT`], or an unknown leading flag.
fn parse_run_arg(arg: Option<&str>) -> Option<RunSpec> {
    let mut rest = arg?;
    let mut timeout = RUN_DEFAULT_TIMEOUT;
    loop {
        if rest.is_empty() {
            return None;
        }
        let Some(after) = rest.strip_prefix("--") else {
            // A leading `-` before the command is an unknown flag; only `--`
            // makes a dash-led command reachable.
            if rest.starts_with('-') {
                return None;
            }
            return Some(RunSpec {
                command: rest.to_string(),
                timeout,
            });
        };
        let after_trimmed = after.trim_start();
        // `--` followed by whitespace (or nothing) IS the terminator: the rest
        // is the command, never a flag — `-- --version` runs `--version`.
        if after.is_empty() || after.starts_with(char::is_whitespace) {
            return (!after_trimmed.is_empty()).then(|| RunSpec {
                command: after_trimmed.to_string(),
                timeout,
            });
        }
        let value_and_rest = after.strip_prefix("timeout")?;
        if !value_and_rest.is_empty() && !value_and_rest.starts_with(char::is_whitespace) {
            return None;
        }
        let value_and_rest = value_and_rest.trim_start();
        let mut parts = value_and_rest.splitn(2, char::is_whitespace);
        let value = parts.next().filter(|v| !v.is_empty())?;
        timeout = parse_run_timeout(value)?;
        rest = parts.next().unwrap_or("").trim_start();
    }
}

/// Parse a `--timeout` value: digits plus an optional `s`/`m`/`h` unit (no
/// unit means seconds), refused when it is not a number, not a whole number of
/// seconds, or past [`RUN_MAX_TIMEOUT`].
fn parse_run_timeout(value: &str) -> Option<std::time::Duration> {
    let (digits, unit) = match value.as_bytes().last() {
        Some(b's') => (&value[..value.len() - 1], 1u64),
        Some(b'm') => (&value[..value.len() - 1], 60),
        Some(b'h') => (&value[..value.len() - 1], 60 * 60),
        _ => (value, 1),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let seconds = digits.parse::<u64>().ok()?.checked_mul(unit)?;
    (seconds <= RUN_MAX_TIMEOUT.as_secs()).then(|| std::time::Duration::from_secs(seconds))
}

/// Collect the ids of every `/skill <id>` token in `text`, in order (spec #652,
/// ticket #654). Each `/skill` token contributes the whitespace-separated token
/// that follows it; an id is kept exactly as typed. The command token is
/// matched case-insensitively like the other commands, and a `/skill` with no
/// token after it contributes nothing.
fn parse_skill_ids(text: &str) -> Vec<String> {
    let mut ids = Vec::new();
    let mut tokens = text.split_whitespace();
    while let Some(token) = tokens.next() {
        if token.eq_ignore_ascii_case("/skill")
            && let Some(id) = tokens.next()
        {
            ids.push(id.to_string());
        }
    }
    ids
}

/// Collect the ids of every `#<id>` in `text`, in order (spec #662). An id is
/// the run of id characters (`[A-Za-z0-9-]`) directly after a `#`, and it counts
/// only when that run starts with an ASCII letter — skill ids are lowercase
/// kebab-case — so `#644` (an issue reference) and a bare `#` or `# 标题` never
/// enter the skill path. The `#` also needs a **left boundary**: the character
/// before it must be absent or outside `[A-Za-z0-9-#]`, so a `#` wedged inside
/// a word (`C#caveman`, `abc#foreman`) and the second `#` of `##foreman` are
/// prose too. The run ends at any other character, so trailing punctuation and
/// CJK text run straight into the token without a space (`#implement-spec,` and
/// `用#caveman试试` still name `implement-spec` / `caveman`). An id is kept
/// exactly as typed; one that matches no registered skill stays in the prompt
/// text verbatim (the dispatch's resolve step decides).
fn parse_hash_skill_ids(text: &str) -> Vec<String> {
    fn is_id_char(c: char) -> bool {
        c.is_ascii_alphanumeric() || c == '-'
    }
    let chars: Vec<char> = text.chars().collect();
    let mut ids = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '#' {
            i += 1;
            continue;
        }
        // The `#` must open a token: a preceding id character means it sits
        // inside a word (`C#`, `abc#`), and a preceding `#` means a markdown
        // heading-ish run (`##foreman`) — neither names a skill.
        if i > 0 && (is_id_char(chars[i - 1]) || chars[i - 1] == '#') {
            i += 1;
            continue;
        }
        // The id run starts right after the `#`; it must be non-empty and lead
        // with a letter, else this `#` is prose (or a markdown heading).
        let start = i + 1;
        let mut end = start;
        while end < chars.len() && is_id_char(chars[end]) {
            end += 1;
        }
        if end > start && chars[start].is_ascii_alphabetic() {
            ids.push(chars[start..end].iter().collect());
        }
        // Skip past the run (or just the `#` when there is none) — no id here
        // can start inside it.
        i = end.max(start);
    }
    ids
}

/// Help text shown for the `/help` command.
pub fn help_text() -> String {
    "\
**cola commands**
`/dir <path> [name]` · Declare a new session in a project directory (created by the next message)
`/topic --worktree [--name <标题>] [--branch <分支>] [--base <ref>]` · Open a new topic whose first message creates a fresh checkout and starts there
`/new --worktree [--name <标题>] [--branch <分支>] [--base <ref>]` · The same, in the current conversation
`/worktree` · Show the pending checkout intent; `/worktree --branch <名字>` or `--base <ref>` changes it before the first message
`/cat <path>` · View a host file in a card (a directory opens the Directory Browser there)
`/diff [--base <ref>]` · Send this branch's repository changes against a base ref as one HTML file
`/run [--timeout <n>[smh]] [--] <cmd>` · Run one shell command on the host and reply with a Terminal Card (also a message starting with `!<cmd>`)
`/run` · Bring the Terminal Card back to the newest position (live run reposted and re-pointed; a settled/lost one rebuilt from its record)
`/switch` · Session card: browse / search / adopt / new
`/switch <kw>` · Switch to a session by name/dir/id (adopts foreign ones)
`/switch <id> [--force]` · Take over a session by id/title
`/switch forget` · Un-map this chat's session (server session stays)
`/sub [list [kw]]` · Child sessions of the current session, read-only (运行中/空闲)
`/sub attach <id|title> [--force]` · Take over one of the current session's direct child sessions into this chat
`/new [name]` · Declare a new session in the current project (created by the next message; no session → default dir)
`/topic [dir] [name]` · Create a new Feishu topic in <dir>; the topic's first message creates the session (bare `/topic` uses the current project)
`/topic --adopt <kw> [--force]` · Open a topic around an existing session
`/name <name>` · Rename current session server-side (on a pending, set the creation title)
`/stop` · Interrupt execution
`/compact` · Compact context
`/card` · Pull the live card back to the newest position, read the Session when no Turn is rendering, or follow a run cola did not start
`/agent <name>` · Switch agent (takes effect next message)
`/model <p/m>` · Switch model (takes effect next message)
`/think [等级]` · Set/clear thinking level (per model; takes effect next message)
`#<id> [#<id> …] [text]` · Load OpenCode skills into this message's prompt (e.g. `#implement-spec 644`); the whole message is submitted verbatim, and an id that matches no skill stays as plain text
`/skill` · Skill picker: list every registered skill (also `/skill <id>`)
`/autoaccept` · Show auto-approve status; `/autoaccept on|off` switches
`/restart` · Restart cola (keeps startup args + log redirect)
`/restart-opencode` · Restart the OpenCode server (an owned server or the V2 managed service)
`/update` · Update cola (GitHub Releases self-update; cargo installs get the cargo command)
`/version` · Show cola version & build provenance (release / crates.io / dev build)
`/status` · Show this conversation's current model/effort, next-turn pending, and session summary
`/help <command>` · Show help for one command (e.g. `/help model`)

话题规则：已绑定会话的话题里，`/switch`、`/switch <关键词>`、`/switch <id> --force`、`/new`、`/dir`、`/sub attach` 被拒绝，请回主对话操作；`/switch forget` 例外——它只解绑、不选会话，已绑定话题正是该用它丢掉自己映射的地方（服务器会话保留，之后话题回到未绑定状态）。从未绑定过会话的话题可以用上面那些命令绑定该话题的唯一会话。
    "
    .to_string()
}

/// Detailed help for one command (`/help <command>`). `None` for unknown names.
pub fn command_help(name: &str) -> Option<String> {
    let text = match name.to_lowercase().as_str() {
        "cat" => {
            "/cat <path>\nOpen one host file as a File Viewer card. cola reads the bytes itself — no model, no server round-trip — so behaviour is identical on every Generation. The card names the file it shows (its RESOLVED path, size and type) and renders a text file as a fenced code block carrying the extension's language hint; an extension the platform has no token for degrades honestly to plain_text rather than being mislabelled. A delimited-text file (.csv/.tsv) previews as a markdown table, because a table is what those bytes are. The card's 「📂 所在目录」 opens the Directory Browser at the file's parent directory.\n- `/cat <directory>` — open the Directory Browser at that directory instead of erroring: `/cat` always means \"show me this path\"\n- `/cat` (no arg) — this help\nPath Scope: `~` expands, a relative path resolves against this session's directory (the conversation's current project while the session is still pending), and an absolute path — inside or outside that directory — is used as-is. The scope is a privacy hint, not a sandbox: whoever can read the chat can read the card, and who may act is the Access List's call.\nA path that does not exist, one cola cannot read, and an empty file each answer distinctly, naming the path.\nExamples: `/cat src/main.rs`, `/cat ~/notes.md`, `/cat /etc/hosts`"
        }
        "diff" => {
            "/diff [--base <ref>]\nRender this branch's repository review as one self-contained HTML File Message: committed changes since the fork point, uncommitted tracked changes, and untracked files. It compares the working tree against `git merge-base <base> HEAD` (never fetches, never touches the index), so the result is stable and re-checkable across turns and restarts.\n- `/diff` — use `--base`, else `[review] base`, else `origin/main`, else `main`\n- `/diff --base <ref>` — override the base for this call\nAny other argument answers this usage line and runs no git. The command runs in this conversation's project directory (the same rule as `/cat`), and the file is replied to the command message so it stays in its chat/topic. Only one `/diff` per chat per 10 seconds; earlier repeats answer 稍后再试. An empty review answers one line and sends no file; each failure (not a git repository, base not found, git missing) answers its own readable text.\nExample: `/diff --base main`"
        }
        "run" => {
            "/run [--timeout <n>[smh]] [--] <cmd>\nRun one shell command on the HOST machine and reply with a Terminal Card: the resolved cwd, the command, its merged stdout/stderr, the exit code and the elapsed time. No model is in the loop — this is a terminal, not a prompt — so nothing enters the session transcript.\n- `/run <cmd>` — one command, one `sh -c`; there is no persistent shell, so `cd`/`export` do not carry between runs. Path Scope: the session's directory, the current project while the session is still pending, else the default session directory; the card names the cwd it actually used.\n- `/run` (no arg) — bring this conversation's Terminal Card back to the newest position, the Terminal family's `/card`: a LIVE run's card is reposted at the bottom and the run keeps updating the NEW card (the old one is frozen as 「已下移」 with its controls gone), while a settled or lost run's card is rebuilt from its Terminal Run Record. Use it when command replies and other messages have buried the card — to read the output or to reach its 终止/继续.\n- `--timeout <n>[smh]` — this run's cap (default 10 minutes, maximum 24 hours). Flags are parsed only before the command, and `--` ends flag parsing so a command may begin with `-`.\n- `!<cmd>` — the same act from a message prefix: a message whose FIRST character is `!` runs the rest as the command, e.g. `!ls -la`. The `!` form accepts no flags — everything after `!` is the command, so `!ls #caveman` literally runs `ls #caveman` and loads no Skill. `!` alone, `! `, and a `!` anywhere but the first character stay prose for the model.\nThere is no pseudo-terminal: `/run` is line-oriented, so a full-screen or interactive program either fails fast (`top` → `failed tty get`, `git commit` → `unable to start editor`) or waits on input that never comes. Use its non-interactive flag (`top -b`, `git commit -m …`), or run it under `tmux` and capture the pane.\nWindows refuses: cola cannot safely terminate the whole process tree there (ADR-0086), so `/run` and `!` answer a refusal and execute nothing.\nThe output is command output, not a sandbox: it can read files, expose environment variables and change the working tree, and every member of the chat can read the card.\nExamples: `/run echo hi`, `/run --timeout 30s cargo test`, `/run -- --version`, `!ls #caveman`"
        }
        "dir" => {
            "/dir <path> [name]\nDeclare a new session rooted at <path>: nothing is created yet — the conversation's next non-command message creates the session in that directory and maps it here, so a mistaken `/dir` can be corrected with another `/dir`, `/new`, `/switch` or `/topic` and leaves no session behind.\n- `/dir` (no arg) — Recent Directories card: pick a recently-used folder and declare it there, or open it as a fresh topic (each row's 建话题 = `/topic <dir>` without typing; only from the main conversation). When there are many directories, the card carries a search box over the paths. Its 📁 浏览目录 button opens a filesystem browser: drill into subdirectories, type a path (前往 browses it, 选择 declares it), toggle dot-directories, and filter the child names.\nExample: `/dir /root/proj/lib`"
        }
        "worktree" => {
            "/worktree\nShow the pending checkout intent: the target directory, branch, the resolved base `ref @ sha`, and the source directory. It is only meaningful while a Pending Session declared by `/topic --worktree` or `/new --worktree` exists; the first message creates the checkout, so an abandoned pending creates nothing.\n- `/worktree --branch <branch>` — change the full branch (a repository-style `<type>/<slug>` is fine; the directory is its last segment)\n- `/worktree --base <ref>` — re-resolve the start ref to a SHA\nBoth are refused after the checkout exists (the directory is fixed). The old read-only inventory card is retired: list checkouts with `/run git worktree list`, and enter an existing one with `/dir <path>`.\nThe base ladder is `[worktree] base`, then `origin/HEAD`, `origin/main`, `origin/master`, `main`, `master`; an unresolvable explicit base is refused, never silently replaced.\nExamples: `/topic --worktree --name 修复登录 --branch feat/fix-login`, `/new --worktree --name Fix Login`, `/worktree --base origin/main`"
        }
        "switch" => {
            "/switch [action]\nSession management card and text forms.\n- `/switch` (no arg) — interactive session card (browse / search / adopt / new)\n- `/switch <keyword>` — switch by title/directory/id; the current chat's sessions win, otherwise a unique global match is adopted. Ambiguous keywords list candidates; no match opens the card pre-filtered by the keyword.\n- `/switch <id|title> [--force]` — take over a session (exact id → id-prefix → title; the card's short hash works too; reject if owned by another chat unless `--force`, or use the card's 强制接管 button)\n- `/switch forget` — un-map this chat's session (server session stays); this is the one `/switch` form a bound topic accepts, because it unbinds instead of selecting\nExamples: `/switch backend`, `/switch ses_abc --force`"
        }
        "sub" => {
            "/sub [list [keyword]] | /sub attach <id|id-prefix|title> [--force]\nShow the child sessions of the current Active Session, read-only: each row carries the child's title, id tail, agent, last activity and live run state (运行中 when Busy/Retry, 空闲 when Idle — one status read per listed row). The view is scoped to THIS session: children of other sessions never appear, and only direct children are listed (no nested descendants, no transcripts).\n- `/sub` / `/sub list` — the child-session card\n- `/sub list <keyword>` — the same card pre-filtered (title/directory/id, whitespace-token AND)\n- `/sub attach <id|id-prefix|title> [--force]` — take over one of the current session's DIRECT children into this chat (the child becomes the Active Session and the takeover confirms in ONE text line carrying its status — no card; pending permissions/questions surface as their own cards). The query resolves like `/switch` (exact id → unique id-prefix → title substring) but only among this session's direct children: an unknown or ambiguous query is reported, and a session that is not a direct child is refused. If the child is mapped to another chat, the owner is named and it is refused unless `--force` (which steals the mapping). Taking over a running child is allowed and the line shows its live state; re-running it for the already-active child changes nothing. The parent stays mapped — `/switch` switches back to it.\nThe card's search box and pagination keep the active keyword/page through every rebuild (six rows per page, ADR-0052). Without an Active Session (a fresh chat, or a Pending Session declared by `/new`/`/dir`/`/topic`) the card opens with a plain empty state.\nExamples: `/sub`, `/sub list 渲染`, `/sub attach 1a2b3c4`, `/sub attach 渲染 --force`"
        }
        "attach" => {
            "/switch <id|title> [--force]\nTake over a session created outside Feishu into this chat. Resolution: exact id → unique id-prefix (the short hash shown on the card works too) → unique title substring. If the session already belongs to another chat, show its owner and reject unless `--force`.\nExample: `/switch ses_abc123`"
        }
        "forget" => {
            "/switch forget\nUn-map this chat's session. The server session stays untouched and can be adopted again.\nExample: `/switch forget`"
        }
        "new" => {
            "/new [name]\nDeclare a new session in the current project (the active session's directory, or the pending's when one is already declared); with neither, the default directory (`work_dir` or cwd). Nothing is created yet: the conversation's next non-command message creates the session and maps it here, so a mistaken `/new` can be corrected with another `/new`, `/dir`, `/switch` or `/topic` and leaves no session behind. The optional name becomes the created session's server title.\n\n/new --worktree [--name <标题>] [--branch <路径名>] [--base <ref>]\nDeclare the same pending with a checkout intent: the first message creates a linked checkout under `[worktree] root` (default `.worktrees`) on branch `<branch>` and starts the session there. The checkout does not exist until then, so an abandoned conversation leaves nothing. `--name` is the session title (any language); `--branch` is the full branch name (optionally `<type>/<slug>`; the directory is its last segment; omitted: derived from an ASCII title, else a `YYYYMMDD-1` token); `--base` overrides the integration ladder and is resolved to a SHA now. While pending, `/worktree` shows the target and `/worktree --branch` / `--base` adjust it.\nExamples: `/new api-refactor`, `/new --worktree --name Fix Login`, `/new --worktree --name 修复登录 --branch feat/fix-login --base origin/main`"
        }
        "topic" => {
            "/topic [dir] [name]\nCreate a real Feishu topic for a new session. The topic is UI-separated from the current conversation, so you can switch between topics in the Feishu client. Opening the topic creates NO session yet: the topic's first non-command message creates the session in the chosen directory and maps it here, so a mistaken `/topic` leaves nothing in the shared store — correct it in place with `/switch <id>` (or `/dir`/`/new`) inside the topic.\n- `/topic` (no args) — new session in the CURRENT PROJECT (the active session's directory, like `/new`; falls back to the default directory when the conversation has no session)\n- `/topic <dir>` — new session rooted at <dir>\n- `/topic <dir> <name>` — also name the session\n\n/topic --worktree [--name <标题>] [--branch <路径名>] [--base <ref>]\nOpen a topic whose first message creates a linked checkout in the current project and starts the session there — the one-command form of \"create a worktree, then a topic in it\". The checkout is created lazily, so closing the topic without a message leaves nothing. `--name` is the title (any language); `--branch` is the full branch name (the directory is its last segment); `--base` overrides the base ladder. The cover names the target, branch and `base @ sha` before the first message; `/worktree` shows and adjusts it while pending.\nExample: `/topic /root/proj/lib api-refactor`, `/topic --worktree --name 修复登录 --branch feat/fix-login`\n\n/topic --adopt <keyword> [--force]\nOpen a topic around an EXISTING session instead of creating a new one. Resolution: exact id → unique id-prefix (the short hash shown on the card works too) → unique title substring (the whole remaining arg is the keyword, so multi-word titles match). Child (sub-task) sessions are rejected. If the session belongs to another chat, reject unless `--force` (which steals the mapping). No argument pops the session card — each row's 建话题接管 button does the same, and an occupied session offers a 强制建话题接管 confirmation.\nExample: `/topic --adopt 重写登录模块`"
        }
        "name" => {
            "/name <name>\nRename the current session server-side (visible to every client sharing the store). With no session yet (a Pending Session declared by `/new`/`/dir`/`/topic`), there is nothing to PATCH: the name becomes the created session's title instead, and a pending topic's cover card updates right away.\nExample: `/name frontend`"
        }
        "stop" => {
            "/stop\nInterrupt the current execution (aborts the running prompt, e.g. a stuck question or tool)."
        }
        "compact" => {
            "/compact\nCompact the current session's context: summarize older messages to free context window."
        }
        "card" => {
            "/card\nBring this Session's card to the newest position, read the Session when no Turn is rendering, or follow a run cola did not start (ADR-0100).\n- With a live Turn (or an external follow) owning the card, it splits the Card Chain at this command: the previous card is finalized with the 部分完成，继续中 header and keeps everything streamed before the pull, and a continuation card replied to this message becomes the newest live card. Use it after command replies, which deliberately stay the newest messages and bury the card above them.\n- With nothing rendering the Session, it reads the Session once. If the read says the Session is busy, or waiting on background tasks, AND the newest message was not written by cola, the posted card follows that run to its end instead of freezing: the run's continuation lands on the card you just asked for. A run cola itself started stays static, and a static read's status line never promises an update it cannot deliver.\n- Otherwise the read stays a one-shot read-only 会话 card: the status line (等待你的确认 > 等待后台任务 > 运行中 > 需要重试 > 空闲), how many requests are waiting for you (their own cards hold the buttons; the pointer names 「见原卡片」), and the last four messages with any Compaction marker. The read carries no button, no form and no Turn Footer; a later `/card` supersedes it (the previous card is marked 已由新卡片接管 · 已停止更新), while a Turn that ran in between leaves it alone.\nThe command always answers — the old 「当前没有正在运行的实时卡片」 notice is retired — except in a conversation with no Session at all."
        }
        "agent" => {
            "/agent <name>\nSwitch the agent for the current session — a per-session override sent on the NEXT message (the OpenCode server has no agent-switch endpoint). Without an override the server's default agent applies; the card (`/agent` alone) shows the current one. `--reset` clears the override back to the server default. Persisted across restarts. On a Pending Session (`/new`/`/dir`/`/topic` before its first message) the override is recorded on the pending and applies to the session the first message creates. Unknown agent names surface as an error on the next prompt.\nExample: `/agent build`"
        }
        "model" => {
            "/model <provider/model>\nSwitch the model for the current session. OpenCode applies it to the session — V2 switches the stored model server-side (durable), V1 sends the recorded override with every prompt (visible from the next message). Codex has no stable switch (only the experimental `thread/settings/update`, which cola does not use), so it records a one-shot pick for the next NEW message: `turn/start` carries `model` + `effort`, nothing is re-sent afterwards, a steer, a folded turn and a command do not consume it, and a restart drops a live thread's pick — re-issue `/model` for the model and `/think` if only a level was pending, because `/model` alone does not restore a `/think` level. Unset = the configured default / server default. On a Pending Session (`/new`/`/dir`/`/topic` before its first message) the override is recorded on the pending and applies to the session the first message creates; it is stored until then, so a restart keeps it.\nExample: `/model opencode-go/deepseek-v4-flash`"
        }
        "think" => {
            "/think [等级]\nSet or clear the thinking level for the current session. OpenCode: V2 stores it inside the session's model ref (durable), V1 sends it as `variant` with every prompt (visible from the next message). Codex records a one-shot next-new-message `effort` for a live thread (keeping any pending model), consumed by the next new turn and dropped on a restart — re-issue `/think` to restore a level-only pending. Each model declares its own levels (e.g. `low`/`high`/`minimal`), so there is no universal scale: the card (`/think` alone) lists what the current model supports, and switching to a model that doesn't declare the current level clears it. `--reset` clears the override (= the server's default for the model). On a Pending Session (`/new`/`/dir`/`/topic` before its first message) the level is recorded on the pending and applies to the session the first message creates; it is stored until then, so a restart keeps it.\nExample: `/think high`"
        }
        "autoaccept" => {
            "/autoaccept [on|off]\nShow or switch auto-allowing permission requests for this session (no permission cards). An exception: a permission whose target the card cannot show in full (for example a command past its display budget) is always declined, even under auto-accept — cola never approves text you could not have reviewed. On a Pending Session (`/new`/`/dir`/`/topic` before its first message) the flag is recorded on the pending and applies to the session the first message creates.\nNo arg: show current state. `/autoaccept on` / `/autoaccept off` switch it.\nExample: `/autoaccept`"
        }
        "skill" => {
            "#<id> [#<id> …] [text]\nLoad one or more OpenCode skills into this message's prompt. Each `#<id>` token names one skill — resolved case-insensitively against the registered ids — collected in order (`#implement-spec #foreman 644`), and the whole message text — tokens included — is submitted verbatim, so trailing text becomes the skill's argument (`#implement-spec 644`). A `#` only opens a token at the start of a word, so `C#caveman` and `##foreman` are prose; an id that matches no registered skill is left as plain text (so `#644` stays an issue reference).\n`/skill` opens the skill picker: every registered skill, hidden ones included. `/skill <id>` is an alias for `#<id>`, and the way to type an id the `#` form cannot express (`2fa-helper`, `web_scrape`).\nOn V2 the ids ride the prompt's structured `skills` field (deterministic injection); on V1, which has no such field, cola folds an instruction to load the skill by name with its `skill` tool into the prompt text.\nExample: `#implement-spec 644`"
        }
        "restart" => {
            "/restart\nRestart cola itself, keeping startup args and the log redirect. The new process takes over the singleton lock (passes --replace). Under a systemd unit cola exits and lets `Restart=on-failure` bring it back; elsewhere it re-execs. cola announces in this chat when it's back."
        }
        "restart-opencode" => {
            "/restart-opencode\nRestart the OpenCode server. cola restarts a server IT started, and the V2 managed service (`serve --service`) through `opencode service restart`; any other server is left alone and needs a manual restart."
        }
        "update" => {
            "/update\nUpdate cola to the latest version (ADR-0030).\n- GitHub Releases install: check GitHub Releases; if newer, download, verify (SHA256SUMS), replace the running binary and restart. When running as a systemd unit, the restart hands back to `Restart=on-failure`; otherwise the new process re-execs with --replace.\n- cargo install (`cargo install colark` / `cargo binstall colark`, detected from cargo's install receipt): nothing is replaced; cola reports the crates.io version and the command to run (`cargo install colark`; binstall users `cargo binstall colark`; add `--force` if cargo says already installed).\nCLI equivalent: `cola update [--check]`."
        }
        "version" => {
            "/version\nShow the running cola's version and build provenance (no network).\n- Release build (built at a clean release tag): `cola 0.7.0`\n- crates.io install (`cargo install colark`): `cola 0.8.1 (crates.io)`\n- Dev build (local or untagged): `cola 0.7.0-dev <branch>@<sha> ⚠` — ⚠ means the source tree had uncommitted changes at build time\nUpdate checks compare only the release version, so a dev build is never reported outdated by its dev marker.\nCLI equivalent: `cola --version`."
        }
        "status" => {
            "/status\nShow this conversation's current model and thinking level, the next-turn pending pick (if any), and a short session summary (session, directory, agent, auto-accept). On a Pending Session the creation model is shown instead of a 待应用 line. Codex's live pending is one-shot: it applies to the next new message. A model/level pick does not change 当前 until that turn runs.\nExample: `/status`"
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
// run the parsed slash commands against the shared handles. The card forms the
// commands pop live in the card layer (`feishu::card::command` over the
// structured-input → card-JSON builders in `card::session`, `card::picker` and
// `card::help`); this module keeps the parser, the dispatch match and the
// text-command behavior (spec #298, ticket D).

use crate::bridge::display::id_tail;
use crate::bridge::handles::{CommandHandles, PickReply, classify_pick};
use crate::bridge::turn::{CardOwnership, StopDisposition};
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
    // Resolve the LIVE binary path. `/proc/self/exe` renders as
    // "<path> (deleted)" once the file was replaced by a rebuild or an upgrade
    // while we're running, and spawning that fails with ENOENT; the NEW binary
    // lives at the stripped path, which is exactly what a restart wants (issue
    // #551). A binary that is not on disk at all is an error, not a spawn of a
    // path that cannot exist.
    let mut exe = crate::update::live_exe_path(&std::env::current_exe()?).ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "the running binary is not on disk")
    })?;
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
/// announce the restart in. The path is the layout's (`~/.cola`).
pub(crate) fn restart_notify_path(layout: &crate::state::StateLayout) -> std::path::PathBuf {
    layout.restart_notify()
}

/// The announcement `/restart` and `/update` leave for the successor process
/// at [`restart_notify_path`]: which chat to announce in, plus the command
/// message and its thread. A command issued inside a Topic announces back
/// INSIDE that topic (the handler replies to `message_id`, which lives in the
/// topic), not in the chat lobby.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct RestartNotify {
    pub(crate) chat_id: String,
    /// The command message; `None` in payloads written before topic support.
    #[serde(default)]
    pub(crate) message_id: Option<String>,
    /// The command's thread id — equal to `chat_id` for the lobby.
    #[serde(default)]
    pub(crate) thread_id: Option<String>,
    /// `update` after `/update`; `restart` for a bare `/restart` (including
    /// payloads from older versions that predate the field).
    #[serde(default)]
    pub(crate) kind: RestartKind,
    /// The new version, present only after `/update`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) version: Option<String>,
}

impl RestartNotify {
    fn new(thread_key: &ThreadKey, message_id: &str) -> Self {
        Self {
            chat_id: thread_key.chat_id.clone(),
            message_id: Some(message_id.to_string()),
            thread_id: Some(thread_key.thread_id.clone()),
            kind: RestartKind::Restart,
            version: None,
        }
    }
}

/// Which restart flow wrote a [`RestartNotify`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum RestartKind {
    #[default]
    Restart,
    Update,
}

/// Persist the announcement for the successor process. Best effort: a failed
/// write only means the restart isn't announced.
fn write_restart_notify(layout: &crate::state::StateLayout, notify: &RestartNotify) {
    if let Ok(raw) = serde_json::to_string(notify) {
        let _ = std::fs::write(restart_notify_path(layout), raw);
    }
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

/// Whether a `/think` or `/agent` argument means "clear the override" — only
/// the `--reset` flag. Clearing is a syntax mechanism, never a bare word: a
/// variant or agent literally named `default`/`off`/`reset` stays selectable,
/// and nothing depends on the server's own `default` sentinel (ADR-0020).
/// Defined once so the text handlers and their tests can't drift apart.
pub(crate) fn is_reset_flag(name: &str) -> bool {
    name == "--reset"
}

/// Split the Session's Card Chain at the `/card` command (ADR-0043's pull),
/// shared by the outer pull check and the in-lock re-check inside
/// [`post_idle_card`].
async fn split_card_for_pull(
    cards: &crate::bridge::handles::CardsHandle,
    session_id: &str,
    message_id: &str,
) {
    crate::bridge::turn::Turn::split_card_chain(
        cards,
        session_id,
        message_id,
        crate::bridge::turn::SplitKind::Pull,
    )
    .await;
}

/// The live renderer that owns `session_id`'s card, if any: the Session itself
/// or, for a sub-task child, its nearest ancestor with a live card (the same
/// host predicate the request sweep uses, ADR-0100). Bounded and re-checked:
/// the walk is a server read, and a Turn that ended during it no longer owns a
/// card, so the caller may enter the pull path only on a host that is still
/// running.
///
/// `/card` must ask this before any idle/follow decision. A child mounted in a
/// thread has no renderer of its own, so posting a child read/follow would race
/// the sweep, which hosts the child's requests on the ancestor's card: one
/// request would get two live blocks.
async fn live_card_host(handles: &CommandHandles, session_id: &str, directory: &str) -> Option<String> {
    let timeout_ms = handles
        .external
        .request_timeout_ms
        .load(std::sync::atomic::Ordering::Relaxed);
    let resolved = crate::bridge::bounded_call(&format!("card host ({session_id})"), timeout_ms, async {
        Ok::<_, crate::error::BridgeError>(
            crate::bridge::pollers::inline_host_session(
                &handles.flow.cards,
                &handles.flow.backend,
                session_id,
                Some(directory),
            )
            .await,
        )
    })
    .await
    .and_then(Result::ok)
    .flatten()?;
    if crate::bridge::turn::Turn::is_running(&handles.flow.cards, &resolved).await {
        Some(resolved)
    } else {
        None
    }
}

/// Mark a just-sent read that never became the Session's card: it stays a
/// visible message, so it must not look current. Best effort; a failed PATCH
/// is logged with the untracked id.
async fn mark_untracked_read(
    cards: &crate::bridge::handles::CardsHandle,
    session_id: &str,
    sent: &str,
    card: &serde_json::Value,
) {
    if !crate::bridge::chain::stamp_taken_over(cards, sent, Some(card)).await {
        tracing::warn!(
            "card read: session {session_id} untracked read {sent} could not be stamped taken over"
        );
    }
}

/// The follow arm's cancellation debt: `/card` stamps the previous card BEFORE
/// the arm switches the map, so a cancellation between the two would leave a
/// stamped old card plus a just-sent read nobody tracks. On drop (the command
/// was cancelled) this spawns the same best-effort stamp for the sent read —
/// unless the arm already made it the Session's card, which is the one case
/// where it must stay unmarked. Disarmed on every normal exit once the card is
/// tracked or the explicit failure path has marked it.
struct UntrackedReadOnCancel {
    cards: crate::bridge::handles::CardsHandle,
    session_id: String,
    sent: String,
    card: serde_json::Value,
    armed: bool,
}

impl UntrackedReadOnCancel {
    fn new(
        cards: &crate::bridge::handles::CardsHandle,
        session_id: &str,
        sent: &str,
        card: &serde_json::Value,
    ) -> Self {
        Self {
            cards: cards.clone(),
            session_id: session_id.to_string(),
            sent: sent.to_string(),
            card: card.clone(),
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for UntrackedReadOnCancel {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // The arm may have committed the sent card before the cancellation
        // reached us (its insert is one map critical section). Then the read IS
        // the Session's card and marking it would stamp a live card taken
        // over; only a read the map does not name is untracked.
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let cards = self.cards.clone();
        let session_id = self.session_id.clone();
        let sent = self.sent.clone();
        let card = self.card.clone();
        runtime.spawn(async move {
            if crate::bridge::turn::Turn::card_message_id(&cards, &session_id)
                .await
                .as_deref()
                == Some(sent.as_str())
            {
                return;
            }
            mark_untracked_read(&cards, &session_id, &sent, &card).await;
        });
    }
}

/// `/card`'s second state (ADR-0100): with no renderer owning the Session, read
/// it once and post exactly ONE read-only Session Card.
///
/// The read and the build happen off the card map; the **check, the send and
/// the current-card update** then share the Session's card-write lock, so two
/// near-simultaneous `/card`s serialize and the later one stamps the card the
/// earlier one posted. The stamp applies only while the map still holds that
/// previous card and it is either an **idle read** or an **actionable terminal**
/// (`Error`/`Unreceived`, whose now-dead Retry/重新发起 control must be
/// stripped): a Turn that ran in between has already superseded it (the map
/// holds the Turn's card), and a `Done`/`Stopped` card carries no control, so
/// both are left untouched. The idle card joins no Chain Record, so nothing
/// here records one.
///
/// A **Turn** installs its accumulator under this same lock now, so it cannot
/// replace the card during the send. The map write is still a
/// compare-and-swap: a running card that was already installed before this
/// command took the lock wins, the read is not recorded, and the just-sent
/// message is stamped taken over so it never looks current.
///
/// A read that says **busy** or **waiting** and whose newest user message is not
/// cola-authored follows the run instead (ADR-0100): the prepared follow — its
/// accumulator, static lead and seed set — is built before the lock, and the
/// same critical section sends the card, inserts the render-owned `CardSession`
/// carrying the sent id and writes its chain record. Nothing reaches the card
/// map before that insert, so a command cancelled earlier leaves no Idle card
/// behind. The seed is attached after the commit (the blocks need a card to
/// land on); a request another card already hosts keeps its own card.
async fn post_idle_card(
    handles: &CommandHandles,
    session_id: &str,
    message_id: &str,
) -> crate::error::Result<()> {
    let directory = handles
        .flow
        .sessions
        .directory_for_session(session_id)
        .await
        .unwrap_or_default();
    // `/card` always answers, and the two reads are bounded SEPARATELY: a
    // gather timeout posts the best-effort static read (no status, no tail),
    // while a prepare timeout keeps the good gather and posts the static read
    // from it. A server that accepts TCP but never replies cannot hang the
    // command.
    let timeout_ms = handles
        .external
        .request_timeout_ms
        .load(std::sync::atomic::Ordering::Relaxed);
    let gathered = crate::bridge::bounded_call("card read", timeout_ms, async {
        Ok::<_, crate::error::BridgeError>(
            crate::bridge::snapshot::gather_snapshot(&handles.flow.backend, session_id, &directory).await,
        )
    })
    .await;
    let (data, gathered_ok) = match gathered {
        Some(Ok(data)) => (data, true),
        _ => {
            tracing::warn!(
                "card read: session {session_id} read timed out; posting the best-effort static read"
            );
            (
                crate::bridge::snapshot::SnapshotData {
                    session_id: session_id.to_string(),
                    directory: directory.clone(),
                    status: None,
                    waiting: false,
                    pending: Vec::new(),
                    tail: Vec::new(),
                    newest_user_anchor: None,
                    newest_user_is_cola_authored: false,
                },
                false,
            )
        }
    };
    let mut follow = if gathered_ok {
        let prepared = crate::bridge::bounded_call("card follow prepare", timeout_ms, async {
            Ok::<_, crate::error::BridgeError>(
                handles
                    .external
                    .prepare_snapshot_follow(&handles.flow, &data)
                    .await,
            )
        })
        .await;
        match prepared {
            Some(Ok(follow)) => follow,
            _ => {
                tracing::warn!(
                    "card read: session {session_id} follow prepare timed out; posting the static read"
                );
                None
            }
        }
    } else {
        None
    };
    // The follow's identity line names the Session the same way an adoption
    // does; the read itself never carried a title.
    let title = if follow.is_some() {
        handles
            .external
            .session_title(&handles.flow.backend, session_id, &directory)
            .await
    } else {
        String::new()
    };
    let cards = &handles.flow.cards;
    let write_lock = cards.write_lock(session_id).await;
    let guard = write_lock.lock().await;
    // A renderer may have taken the card over between the pull check and this
    // critical section — the Session's own, or an ancestor's for a sub-task
    // child (the sweep hosts the child's requests there). Either way the
    // render-owned meaning wins, and `/card` pulls that card instead of
    // posting a second host; the split is entered after the guard is released
    // (it takes the host's lock, never nested under this one).
    if let Some(host) = live_card_host(handles, session_id, &directory).await {
        drop(guard);
        split_card_for_pull(cards, &host, message_id).await;
        return Ok(());
    }
    let previous = crate::bridge::turn::Turn::card_to_supersede(cards, session_id).await;
    // The pointer, the send, the arm and the seed are one critical section:
    // the seed is filtered HERE (so no sweep can surface one between the
    // decision and the attach), and the card's count names exactly that set.
    //
    // The card SENT is always the static idle read (ADR-0100): if this command
    // dies before the arm, or the arm refuses, what stays in the chat must not
    // promise updates or claim the pendings ride it. The renderer's first flush
    // paints the follow lead afterwards, once a renderer actually exists.
    let card = crate::feishu::card::session::build_idle_card(&data);
    if let Some(plan) = &mut follow {
        handles
            .external
            .resolve_snapshot_follow_seeds(&handles.flow, plan)
            .await;
        handles
            .external
            .compose_snapshot_follow_lead(plan, "读取", &title, &data);
    }
    // Deliberately UNBOUNDED: Feishu's create has no idempotency key, so a
    // timeout around a create whose response is slow could abandon a card that
    // did land, leaving it untracked for a retry to duplicate. A real send
    // error takes the ordinary failure path; the bounded reads and stamp
    // PATCH are the lock's other long awaits (ADR-0100).
    let sent = handles.flow.platform.reply_card(message_id, &card).await?;
    if let Some(plan) = &mut follow {
        // From here on the command owes the just-sent read an ending if it is
        // cancelled before the arm makes it the Session's card: the previous
        // card may already be stamped (below), so a dropped future must still
        // mark the read instead of leaving it looking current.
        let mut untracked = UntrackedReadOnCancel::new(cards, session_id, &sent, &card);
        // The old card is neutralised BEFORE the arm switches the map: it
        // must never stay the visible card while the follow (or a later idle
        // read) becomes current. A failed stamp aborts the command and keeps
        // the old card current, so exactly one card is current and the next
        // `/card` retries. The stamp is under the lock for that ordering; it
        // is a bounded GET + PATCH on another message.
        if let Some(previous) = &previous {
            let stamped = crate::bridge::chain::stamp_taken_over(cards, previous, None).await;
            if !stamped {
                // The old card could not be marked. Keep it current so exactly
                // one card is current; the posted read stays untracked and the
                // next `/card` retries the stamp.
                drop(guard);
                // The old card stays current, but the just-sent read must not
                // be indistinguishable from it: mark the untracked read too.
                mark_untracked_read(cards, session_id, &sent, &card).await;
                untracked.disarm();
                tracing::warn!(
                    "card read: session {session_id} could not stamp the previous card {}; leaving it current and marking the new read",
                    previous
                );
                return Ok(());
            }
        }
        // The send awaited; a Turn may have installed itself meanwhile. The
        // arm's own compare-and-swap refuses a running card, so the read is not
        // recorded and the just-sent message is stamped taken over.
        let armed = handles
            .external
            .arm_snapshot_follow_locked(&handles.flow, session_id, &sent, plan, true)
            .await;
        if armed {
            // A committed arm makes the sent read the Session's card: the
            // cancellation debt is settled, so drop it before anything else
            // can observe the command as cancelled.
            untracked.disarm();
            // Only a committed follow adopts a new run, so that is the only
            // branch that may drop a pre-run `/stop`; a refused arm leaves the
            // marker for the run that actually owns the card.
            handles
                .flow
                .waits
                .stopped_sessions
                .lock()
                .await
                .remove(session_id);
        }
        drop(guard);
        if armed {
            // The arm already spawned the renderer right after its map insert
            // (before the seed/record awaits), so a cancellation anywhere after
            // that point leaves a live card with a render loop.
            tracing::info!("card read: session {session_id} follows the running turn on the posted card");
        } else {
            mark_untracked_read(cards, session_id, &sent, &card).await;
            untracked.disarm();
            tracing::info!(
                "card read: session {session_id} the old card kept the session during the follow read; the posted read is stamped"
            );
        }
        return Ok(());
    }
    // Neutralise the old card BEFORE the map switch, for the same reason as the
    // follow branch: a failed stamp keeps the old read current instead of
    // recording a second unmarked card.
    if let Some(previous) = &previous {
        let stamped = crate::bridge::chain::stamp_taken_over(cards, previous, None).await;
        if !stamped {
            // No predecessor is ever left both unmarked and current: keep the
            // old read current, leave the just-sent read untracked, and let the
            // next `/card` retry the stamp.
            drop(guard);
            // The old read stays current; the just-sent read is visibly
            // superseded so a third `/card` cannot mistake it for current.
            mark_untracked_read(cards, session_id, &sent, &card).await;
            tracing::warn!(
                "card read: session {session_id} could not stamp the previous card {}; leaving it current and marking the new read",
                previous
            );
            return Ok(());
        }
    }
    // The send awaited; a Turn may have installed itself meanwhile. The CAS in
    // `record_idle_card` refuses a running card, and the read then stays a
    // plain message marked superseded — never the Session's card, never a host
    // for a later flush.
    let recorded = crate::bridge::turn::Turn::record_idle_card(cards, session_id, &sent, message_id).await;
    drop(guard);
    if recorded {
        tracing::info!("card read: session {session_id} idle Session Card posted");
    } else {
        mark_untracked_read(cards, session_id, &sent, &card).await;
        tracing::info!(
            "card read: session {session_id} the old card kept the session during the read; the posted read is stamped"
        );
    }
    Ok(())
}

/// Execute a parsed slash command against the shared handles. Unrecognized
/// `/command`s are intercepted by the message coordinator (which owns the
/// prompt pipeline), so this never forwards — the `Command::Forward` arm below
/// is unreachable and kept only for exhaustiveness.
#[allow(clippy::too_many_arguments)] // the dispatcher carries the message's request facts
pub(crate) async fn handle_command(
    handles: &CommandHandles,
    cmd: Command,
    thread_key: ThreadKey,
    message_id: &str,
    kind: ConversationKind,
    requester_open_id: Option<&str>,
    is_group: bool,
) -> crate::error::Result<()> {
    // Topic command gate (ADR-0007, ADR-0023): the rule lives in
    // `Command::topic_rejection`, so every command — including future ones —
    // is restricted by one table from this single call site.
    if kind == ConversationKind::Topic {
        let has_session = handles
            .flow
            .sessions
            .store
            .lock()
            .await
            .get_active(&thread_key)
            .is_some();
        if let Some(reason) = cmd.topic_rejection(has_session) {
            handles.flow.platform.reply_text(message_id, reason).await?;
            return Ok(());
        }
    }
    match cmd {
        Command::Dir(path) => {
            let Some(dir_str) = resolve_directory_or_reply(
                &*handles.flow.platform,
                message_id,
                &path,
                "`/dir`；或先用 `/new` 在默认目录新建会话。",
            )
            .await?
            else {
                return Ok(());
            };
            // Lazy Session Creation (ADR-0041): `/dir` records a Pending
            // Session rooted at `path` instead of creating a backend session;
            // the conversation's first non-command message materialises it.
            // Repeating the command (or `/new`, `/switch`, `/topic`) before
            // that replaces the pending, so a corrected directory leaves no
            // trace in the shared store.
            let pending = handles
                .flow
                .sessions
                .declare_pending(&thread_key, dir_str, None)
                .await?;
            handles
                .flow
                .platform
                .reply_text(
                    message_id,
                    &format!("下一条消息将在目录 `{}` 创建会话。", pending.directory),
                )
                .await?;
        }
        Command::DirCard => {
            crate::feishu::card::command::send_dir_card(handles, &thread_key, message_id).await?;
        }
        Command::WorktreeStatus => {
            crate::bridge::worktree::status(handles, &thread_key, message_id).await?;
        }
        Command::WorktreeAdjust { branch, base } => {
            crate::bridge::worktree::adjust(handles, &thread_key, message_id, branch, base).await?;
        }
        Command::WorktreeUsage => {
            handles
                .flow
                .platform
                .reply_text(message_id, crate::bridge::worktree::WORKTREE_USAGE)
                .await?;
        }
        Command::Cat(path) => {
            crate::feishu::card::command::send_cat_card(handles, &thread_key, message_id, path.as_deref())
                .await?;
        }
        Command::Diff(base) => {
            crate::bridge::repo_diff::run(handles, &thread_key, message_id, base).await?;
        }
        Command::DiffUsage => {
            handles
                .flow
                .platform
                .reply_text(message_id, crate::bridge::repo_diff::DIFF_USAGE)
                .await?;
        }
        Command::Run(spec) => {
            crate::bridge::terminal::execute(
                handles,
                &thread_key,
                crate::bridge::terminal::TerminalRequest {
                    message_id,
                    requester_open_id,
                    is_group,
                },
                spec,
            )
            .await?;
        }
        Command::Switch(action) => {
            handle_switch_action(handles, &thread_key, action, message_id, kind).await?;
        }
        Command::Sub(action) => match action {
            SubAction::List(keyword) => {
                crate::feishu::card::command::send_child_card(handles, &thread_key, &keyword, message_id)
                    .await?;
            }
            SubAction::Attach { query, force } => {
                handle_sub_attach(handles, &thread_key, &query, force, message_id, kind).await?;
            }
        },
        Command::New(name) => {
            // Lazy Session Creation (ADR-0041): `/new` records a Pending
            // Session in the current project (the pending's, else the active
            // session's directory — ADR-0012) instead of creating a backend
            // session; the first non-command message materialises it. A
            // mistaken `/new` therefore leaves nothing in the shared store.
            // The creation-title policy (ADR-0007) applies at materialisation.
            let pending = handles
                .declare_pending_in_current_project(&thread_key, name.clone())
                .await?;
            let reply = match &pending.title {
                Some(n) => format!("下一条消息将创建会话「{}」（目录 `{}`）。", n, pending.directory),
                None => format!("下一条消息将创建会话（目录 `{}`）。", pending.directory),
            };
            handles.flow.platform.reply_text(message_id, &reply).await?;
        }
        Command::NewWorktree { name, branch, base } => {
            // ADR-0095: the same new-conversation decision as `/new`, plus a
            // checkout intent. Only the intent is written here; the first
            // message creates the checkout and the Session in it.
            //
            // Capture the pending identity BEFORE the Git work: a `/dir`,
            // `/new` or `/topic` that lands while we resolve the intent must
            // win, not be overwritten by this earlier command.
            let expected_epoch = handles.flow.sessions.pending_epoch_for(&thread_key).await;
            let source = handles.current_project_directory(&thread_key).await;
            let (intent, root) = match crate::bridge::worktree::intent(
                &source,
                handles.config.worktree_root(),
                name.as_deref(),
                branch.as_deref(),
                base.as_deref(),
                handles.config.worktree_base(),
            )
            .await
            {
                Ok(prepared) => prepared,
                Err(error) => {
                    handles
                        .flow
                        .platform
                        .reply_text(message_id, &format!("不能在当前目录创建 worktree：{error}"))
                        .await?;
                    return Ok(());
                }
            };
            let root_ignored = crate::git::root_is_ignored(&source, &root).await;
            // Declare against the source the intent was resolved from — never
            // re-read the current project here — and only if no newer pending
            // replaced it while Git ran.
            let pending = match handles
                .flow
                .sessions
                .declare_pending_worktree_if_unchanged(
                    &thread_key,
                    expected_epoch,
                    source.clone(),
                    name.clone(),
                    intent.clone(),
                )
                .await?
            {
                Some(pending) => pending,
                None => {
                    handles
                        .flow
                        .platform
                        .reply_text(
                            message_id,
                            "⚠️ 这个对话的待建会话在这条命令执行期间已经变了（可能刚发了 `/dir`、`/new` 或另一个 `/topic`），这次 worktree 意图没有覆盖它；请重试。",
                        )
                        .await?;
                    return Ok(());
                }
            };
            let reply = crate::bridge::worktree::declared_reply(
                pending.title.as_deref(),
                &source,
                &intent,
                &root,
                root_ignored,
            );
            handles.flow.platform.reply_text(message_id, &reply).await?;
        }
        Command::Topic { directory, name } => {
            // Bare `/topic` (directory: None) inherits the conversation's
            // current project, exactly like `/new`; an explicit directory goes
            // through the same existence check as `/dir`.
            let dir_str = match directory {
                Some(dir) => {
                    let Some(d) =
                        resolve_directory_or_reply(&*handles.flow.platform, message_id, &dir, "`/topic`。")
                            .await?
                    else {
                        return Ok(());
                    };
                    d
                }
                None => handles.current_project_directory(&thread_key).await,
            };
            // Open the topic in one transaction (ADR-0041): the cover card
            // (ADR-0023), in-topic seed and Pending Session all live in
            // `bridge::topic`; no server session is created here — the topic's
            // first non-command message materialises the pending.
            match crate::bridge::topic::open_topic(
                &handles.topic_handles(),
                &thread_key.chat_id,
                message_id,
                crate::bridge::topic::TopicOpening::Fresh {
                    directory: dir_str,
                    name,
                    worktree: None,
                },
            )
            .await
            {
                Ok(opened) => {
                    tracing::info!(
                        "topic: opened topic {} with a pending session in chat {}",
                        opened.thread_id,
                        thread_key.chat_id
                    );
                }
                Err(crate::bridge::topic::OpenTopicError::NoThreadId) => {
                    handles.flow.platform
                        .reply_text(
                            message_id,
                            "⚠️ 当前会话不支持创建话题（未返回 thread_id）。请改用 `/dir <目录>` 或在飞书里手动创建话题。",
                        )
                        .await?;
                }
                Err(crate::bridge::topic::OpenTopicError::Failed(e)) => return Err(e),
            }
        }
        Command::TopicWorktree { name, branch, base } => {
            // ADR-0095: creation is a flag on the new-conversation flow. The
            // pending's directory stays the SOURCE, so every pending-window
            // command reads where the conversation stands; the checkout is
            // created by the first message from the stored intent.
            let source = handles.current_project_directory(&thread_key).await;
            let (intent, _root) = match crate::bridge::worktree::intent(
                &source,
                handles.config.worktree_root(),
                name.as_deref(),
                branch.as_deref(),
                base.as_deref(),
                handles.config.worktree_base(),
            )
            .await
            {
                Ok(prepared) => prepared,
                Err(error) => {
                    handles
                        .flow
                        .platform
                        .reply_text(message_id, &format!("不能在当前目录创建 worktree：{error}"))
                        .await?;
                    return Ok(());
                }
            };
            match crate::bridge::topic::open_topic(
                &handles.topic_handles(),
                &thread_key.chat_id,
                message_id,
                crate::bridge::topic::TopicOpening::Fresh {
                    directory: source,
                    name,
                    worktree: Some(intent),
                },
            )
            .await
            {
                Ok(opened) => {
                    tracing::info!(
                        "topic: opened worktree topic {} with a pending session in chat {}",
                        opened.thread_id,
                        thread_key.chat_id
                    );
                }
                Err(crate::bridge::topic::OpenTopicError::NoThreadId) => {
                    handles.flow.platform
                        .reply_text(
                            message_id,
                            "⚠️ 当前会话不支持创建话题（未返回 thread_id）。请改用 `/new --worktree` 或在飞书里手动创建话题。",
                        )
                        .await?;
                }
                Err(crate::bridge::topic::OpenTopicError::Failed(e)) => return Err(e),
            }
        }
        Command::TopicAdopt { keyword, force } => {
            handle_topic_adopt(handles, &thread_key, &keyword, force, message_id).await?;
        }
        Command::TopicAdoptCard => {
            // Reuse the `/switch` session card, whose per-row button now also
            // offers "建话题接管" (ADR-0016). The card action handler creates
            // the topic via the card's own `open_message_id`.
            crate::feishu::card::command::send_switch_card(
                handles,
                &thread_key,
                "",
                crate::feishu::card::session::SwitchScope::Directory,
                message_id,
            )
            .await?;
        }
        Command::Name(name) => {
            // `/name` renames the conversation's session (ADR-0007). An active
            // session is PATCHed server-side — visible to every client, and the
            // session-list cache is invalidated so the new title shows immediately;
            // for a cover-rooted topic, patch the cover card right away too
            // (the chat-list topic entry is its content, ADR-0023). On a
            // Pending Session (ADR-0041) there is nothing to PATCH yet: the
            // name becomes the creation title the first message applies.
            if let Some(id) = handles.flow.sessions.get_session_id(&thread_key).await {
                handles.flow.backend.update_session_title(&id, &name).await?;
                handles.flow.sessions.invalidate_cache().await;
                crate::bridge::topic::sync_topic_cover_title(
                    &handles.flow.cards,
                    &handles.flow.sessions,
                    &handles.flow.backend,
                    &id,
                )
                .await;
                handles
                    .flow
                    .platform
                    .reply_text(message_id, &format!("Renamed to \"{}\".", name))
                    .await?;
            } else if handles
                .flow
                .sessions
                .update_pending(&thread_key, |p| p.title = Some(name.clone()))
                .await?
            {
                // ADR-0023 + ADR-0041: on a pending topic the cover card is
                // patched now — no server session exists to sync from yet. The
                // created session gets this title at materialisation.
                crate::bridge::topic::rename_pending_cover(&handles.topic_handles(), &thread_key, &name)
                    .await;
                handles
                    .flow
                    .platform
                    .reply_text(
                        message_id,
                        &format!("已记下标题「{}」——下一条消息创建会话时使用。", name),
                    )
                    .await?;
            } else {
                handles
                    .flow
                    .platform
                    .reply_text(
                        message_id,
                        &format!(
                            "⚠️ {}还没有会话，先用 `/new` 或 `/dir` 创建。",
                            crate::bridge::display::feishu_side_label(&thread_key)
                        ),
                    )
                    .await?;
            }
        }
        Command::AutoAccept(action) => {
            // `Status` reports the current state; `Set(on)` switches the flag
            // AND clears requests that are already pending but were seen
            // before (the poller's `seen` set skips them, so they'd
            // otherwise hang as cards forever). The flag lives on whatever
            // the next prompt will use (ADR-0041): a Pending Session is
            // configured too, it just has no requests to approve yet.
            match action {
                crate::bridge::command::AutoAcceptAction::Status => {
                    crate::feishu::card::command::send_autoaccept_card(handles, &thread_key, message_id)
                        .await?;
                    return Ok(());
                }
                crate::bridge::command::AutoAcceptAction::Set(on) => {
                    let Some((settings, pending_epoch)) = handles
                        .flow
                        .sessions
                        .settings_and_pending_epoch(&thread_key)
                        .await
                    else {
                        handles
                            .flow
                            .platform
                            .reply_text(
                                message_id,
                                &format!(
                                    "⚠️ {}还没有会话，先用 `/new` 或 `/dir` 创建。",
                                    crate::bridge::display::feishu_side_label(&thread_key)
                                ),
                            )
                            .await?;
                        return Ok(());
                    };
                    let mut approved = Vec::new();
                    if on && let Some(id) = settings.session_id.as_deref() {
                        approved = crate::bridge::request::kind::approve_pending_for_session(
                            &handles.flow.sessions,
                            &handles.flow.requests,
                            &handles.flow.backend,
                            id,
                            &settings.directory,
                        )
                        .await;
                        if !approved.is_empty() {
                            // The same residue the card toggle leaves: ONE mode
                            // receipt and the approved blocks dismissed. Without
                            // this the sweep would resolve them as
                            // `⏱ 已由其他客户端处理` — a lie, cola itself
                            // approved them (#193 follow-up). No clicked card,
                            // so `resolve_blocks` patches every card that
                            // renders one of the blocks.
                            crate::bridge::request::delivery::resolve_blocks(
                                &handles.flow.requests.permission,
                                &handles.flow.cards,
                                &handles.flow.requests,
                                &Some(id.to_string()),
                                id,
                                crate::bridge::request::delivery::Origin::Command,
                                &approved,
                                crate::bridge::request::delivery::Residue::Single(
                                    crate::bridge::request::kind::AUTOACCEPT_RECEIPT,
                                ),
                            )
                            .await;
                        }
                    }
                    // Field-only (issue #910): the approval sweep above can
                    // run for a while, and writing the whole snapshot back
                    // would restore the model/variant it read before a
                    // concurrent confirmation recorded a new pair. The epoch
                    // identifies a Pending Session target, so a `/new` that
                    // replaced it is a refusal instead of a flag on the
                    // replacement.
                    if !handles
                        .flow
                        .sessions
                        .set_auto_accept(
                            &thread_key,
                            settings.session_id.as_deref(),
                            pending_epoch.unwrap_or_default(),
                            on,
                        )
                        .await?
                    {
                        handles
                            .flow
                            .platform
                            .reply_text(
                                message_id,
                                "⚠️ 会话状态已变化（可能刚切换/创建），这次操作没有生效。",
                            )
                            .await?;
                        return Ok(());
                    }
                    let state = if on { "开" } else { "关" };
                    let extra = if on && !approved.is_empty() {
                        format!("（已自动批准 {} 条待处理请求）", approved.len())
                    } else {
                        String::new()
                    };
                    handles
                        .flow
                        .platform
                        .reply_text(message_id, &format!("🔁 已将会话自动审批{state}。{}", extra))
                        .await?;
                }
            }
        }
        Command::Stop => {
            if let Some(id) = handles.flow.sessions.get_session_id(&thread_key).await {
                // What the card will do with the stop decides the ack, read
                // BEFORE the marker/interrupt round-trip: the ownership
                // verdict's one classified read (ADR-0070). A render-owned
                // card (a live Turn, follow or external renderer) is stamped
                // 「⏹ 已停止」 by that owner on its next tick (#394) and IS the
                // acknowledgement; a yielded card has no render owner and only
                // settles at the quiet true end (ADR-0060), so it needs an ack
                // naming that ending; anything else (no card, a spent chain)
                // will never show the stop. The pre-stop read is deliberate —
                // after the interrupt a render-owned card may already be
                // stamped, and a post-read could not tell that from a card
                // that was terminal all along.
                let disposition = CardOwnership::read(&handles.flow.cards, &handles.flow.waits, &id)
                    .await
                    .stop_disposition();
                // Mark the session stopped BEFORE the interrupt round-trip: the
                // abort settles the run server-side, and a drain/follow tick
                // that observes that settled run can beat the marker back and
                // finalize Done/Error instead of the stop terminal (#394). The
                // next Turn clears the marker when it starts.
                let waits = &handles.flow.waits;
                let inserted = waits.stopped_sessions.lock().await.insert(id.clone());
                if let Err(e) = handles.flow.backend.interrupt(&id).await {
                    // Roll back only a marker THIS call inserted: a failed
                    // second `/stop` must not wipe the marker an earlier
                    // successful stop left for the still-draining turn.
                    if inserted {
                        waits.stopped_sessions.lock().await.remove(&id);
                    }
                    return Err(e);
                }
                // One reply per state (the consts carry the rationale). The
                // interrupt still ran in every arm: the run may be live
                // server-side even when cola's card is not.
                match disposition {
                    StopDisposition::OwnerStamps => {}
                    StopDisposition::QuietEndAck => {
                        handles
                            .flow
                            .platform
                            .reply_text(message_id, STOPPED_WAITING)
                            .await?;
                    }
                    StopDisposition::Nothing => {
                        handles
                            .flow
                            .platform
                            .reply_text(message_id, NO_RUNNING_TASK)
                            .await?;
                    }
                }
            } else {
                handles
                    .flow
                    .platform
                    .reply_text(message_id, "当前没有正在执行的会话。")
                    .await?;
            }
        }
        Command::Compact => {
            if let Some(id) = handles.flow.sessions.get_session_id(&thread_key).await {
                handles.flow.backend.compact(&id).await?;
                handles.flow.platform.reply_text(message_id, COMPACTING).await?;
            } else {
                handles
                    .flow
                    .platform
                    .reply_text(
                        message_id,
                        &format!(
                            "{}还没有会话，无需压缩。",
                            crate::bridge::display::feishu_side_label(&thread_key)
                        ),
                    )
                    .await?;
            }
        }
        Command::Card => {
            // The pull is the explicit exception to "commands never split the
            // chain" (ADR-0043, 2026-09-22 amendment): mid-turn, command
            // replies bury the live card above them, and this command moves
            // the chain's live continuation back to the newest message.
            let Some(session_id) = handles.flow.sessions.get_session_id(&thread_key).await else {
                handles
                    .flow
                    .platform
                    .reply_text(message_id, NO_SESSION_TO_READ)
                    .await?;
                return Ok(());
            };
            let directory = handles
                .flow
                .sessions
                .directory_for_session(&session_id)
                .await
                .unwrap_or_default();
            // A live renderer owns the card — the Session's own, or an
            // ancestor's for a sub-task child mounted in this thread. Pull
            // that card; posting a child read/follow would race the sweep,
            // which hosts the child's requests on the ancestor (ADR-0100).
            if let Some(host) = live_card_host(handles, &session_id, &directory).await {
                tracing::info!("card pull: session {session_id} live card {host} split requested");
                split_card_for_pull(&handles.flow.cards, &host, message_id).await;
                return Ok(());
            }
            post_idle_card(handles, &session_id, message_id).await?;
        }
        Command::AgentCard => {
            crate::feishu::card::command::send_agent_card(handles, &thread_key, message_id).await?;
        }
        Command::Agent(name) => {
            // `/agent` records the per-session override and — on a generation
            // whose selection is durable (V2) — switches the session's agent,
            // so the next turn uses it with nothing re-sent (the server honors
            // a per-prompt `agent` on V1). On a Pending Session the override is
            // recorded on the pending and lands on the created session at
            // materialisation (ADR-0041). Unknown agent names surface as a
            // clear error on the next prompt's card. `--reset` clears the
            // override (the server's default agent applies — on V2 that means
            // switching to the default agent id explicitly, because V2 has no
            // "unset" arm); an agent literally named
            // `default`/`off`/`reset` is a normal pick, never a clear word.
            let cleared = is_reset_flag(&name);
            let agent = if cleared { None } else { Some(name.clone()) };
            let outcome = handles
                .flow
                .sessions
                .pick_agent(&handles.flow.backend, &thread_key, agent)
                .await;
            match classify_pick(
                outcome,
                "Agent",
                crate::bridge::display::feishu_side_label(&thread_key),
            ) {
                PickReply::Applied { .. } => {
                    let msg = if cleared {
                        "已清除 Agent（回到服务器默认）。".to_string()
                    } else {
                        format!("Agent: {}（下一条消息开始生效）", name)
                    };
                    handles.flow.platform.reply_text(message_id, &msg).await?;
                }
                PickReply::Failed(text) => {
                    handles.flow.platform.reply_text(message_id, &text).await?;
                }
            }
        }
        Command::ModelCard => {
            crate::feishu::card::command::send_model_card(handles, &thread_key, message_id).await?;
        }
        Command::Model(name) => {
            // `/model` records the per-session override and — on a generation
            // whose selection is durable (V2) — switches the session's model, so
            // the next turn uses it with nothing re-sent; V1 sends it as a
            // per-prompt model. Validate the shape up front so a typo gets
            // immediate feedback instead of a silent no-op.
            //
            // The text form obeys the same rule as the card (spec #930): a
            // provider other than the current channel is refused **before any
            // request** when the channel is determinable — a provider is a
            // config fact. An unknown channel does not block: the model is
            // recorded model-only and the server's own error is the fallback.
            let Some(parsed) = crate::backend::parsing::parse_model(&name) else {
                handles
                    .flow
                    .platform
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
            if handles
                .flow
                .sessions
                .session_settings(&thread_key)
                .await
                .is_none()
            {
                handles
                    .flow
                    .platform
                    .reply_text(
                        message_id,
                        &format!(
                            "⚠️ {}还没有会话，先用 `/new` 或 `/dir` 创建。",
                            crate::bridge::display::feishu_side_label(&thread_key)
                        ),
                    )
                    .await?;
                return Ok(());
            }
            match handles
                .flow
                .sessions
                .model_pick_decision(&handles.flow.backend, &thread_key, &parsed.provider_id)
                .await
            {
                crate::bridge::handles::ModelPickDecision::Refuse(text) => {
                    handles.flow.platform.reply_text(message_id, &text).await?;
                }
                crate::bridge::handles::ModelPickDecision::Record {
                    target,
                    provider_fact,
                } => {
                    use crate::bridge::handles::PickTarget;
                    let pending = matches!(target, PickTarget::Pending(_));
                    let outcome = match target {
                        // Bound to the session the decision classified: a
                        // `/switch` since then is a no-op, never a record on
                        // the new session.
                        PickTarget::Live(session_id) => {
                            handles
                                .flow
                                .sessions
                                .pick_model_expecting(
                                    &handles.flow.backend,
                                    &thread_key,
                                    Some(&session_id),
                                    provider_fact,
                                    &name,
                                    None,
                                )
                                .await
                        }
                        // The thread became a Pending Session while the pick
                        // was classified: record the model only; its provider
                        // is the daemon's config fact, not a creation field.
                        PickTarget::Pending(epoch) => {
                            handles
                                .flow
                                .sessions
                                .pick_pending_model(
                                    &handles.flow.backend,
                                    &thread_key,
                                    epoch,
                                    provider_fact,
                                    &name,
                                    None,
                                )
                                .await
                        }
                    };
                    match classify_pick(
                        outcome,
                        "模型",
                        crate::bridge::display::feishu_side_label(&thread_key),
                    ) {
                        PickReply::Applied { cleared_variant } => {
                            let text = if pending {
                                let extra = cleared_variant
                                    .map(|v| format!("（已清除思考等级 `{v}`）"))
                                    .unwrap_or_default();
                                format!("Model: {name}（将在会话创建时生效）{extra}")
                            } else if handles.flow.backend.owns_next_turn_pending() {
                                let extra = cleared_variant
                                    .map(|v| format!("（已清除思考等级 `{v}`：新模型不支持）"))
                                    .unwrap_or_default();
                                format!("Model: {name}（已记录：将在下一条新消息生效）{extra}")
                            } else {
                                let extra = cleared_variant
                                    .map(|v| format!("（已清除思考等级 `{v}`：新模型不支持）"))
                                    .unwrap_or_default();
                                format!("Model: {}（下一条消息开始生效）{}", name, extra)
                            };
                            handles.flow.platform.reply_text(message_id, &text).await?;
                        }
                        PickReply::Failed(text) => {
                            handles.flow.platform.reply_text(message_id, &text).await?;
                        }
                    }
                }
            }
        }
        Command::ThinkCard => {
            crate::feishu::card::command::send_think_card(handles, &thread_key, message_id).await?;
        }
        Command::Think(name) => {
            // `/think` records the per-session variant and — on a generation
            // whose selection is durable (V2) — rewrites the session's model
            // ref with the variant inside it; V1 sends it as a per-prompt
            // `variant`. On a Pending Session the override is recorded on the
            // pending and lands on the created session at materialisation
            // (ADR-0041). `--reset` clears the override (the server's default
            // for the model); a variant literally named
            // `default`/`off`/`reset` is a normal pick, never a clear word.
            let Some(settings) = handles.flow.sessions.session_settings(&thread_key).await else {
                handles
                    .flow
                    .platform
                    .reply_text(
                        message_id,
                        &format!(
                            "⚠️ {}还没有会话，先用 `/new` 或 `/dir` 创建。",
                            crate::bridge::display::feishu_side_label(&thread_key)
                        ),
                    )
                    .await?;
                return Ok(());
            };
            let cleared = is_reset_flag(&name);
            if !cleared
                && let Some(model) = handles
                    .flow
                    .sessions
                    .effective_selection(&handles.flow.backend, &settings)
                    .await
                && let Some(variants) = handles
                    .flow
                    .sessions
                    .model_variants(&handles.flow.backend, &model.provider_id, &model.id)
                    .await
                && !variants.iter().any(|v| v == &name)
            {
                let available = if variants.is_empty() {
                    "（无）".to_string()
                } else {
                    variants.join("、")
                };
                handles.flow.platform
                    .reply_text(
                        message_id,
                        &format!(
                            "⚠️ 当前模型 `{}/{}` 不支持思考等级 `{name}`。可用：{available}。（清除请用 `/think --reset`）",
                            model.provider_id, model.id
                        ),
                    )
                    .await?;
                return Ok(());
            }
            let variant = if cleared { None } else { Some(name.clone()) };
            let outcome = handles
                .flow
                .sessions
                .pick_think(&handles.flow.backend, &thread_key, variant)
                .await;
            match classify_pick(
                outcome,
                "思考等级",
                crate::bridge::display::feishu_side_label(&thread_key),
            ) {
                PickReply::Applied { .. } => {
                    let msg = if cleared {
                        "已清除思考等级（回到模型默认）。".to_string()
                    } else {
                        format!("Thinking: {}（下一条消息开始生效）", name)
                    };
                    handles.flow.platform.reply_text(message_id, &msg).await?;
                }
                PickReply::Failed(text) => {
                    handles.flow.platform.reply_text(message_id, &text).await?;
                }
            }
        }
        Command::Help(target) => {
            match target {
                // No target: the buttonless reference card (ADR-0012, issue 05).
                None => {
                    crate::feishu::card::command::send_help_card(handles, message_id).await?;
                }
                Some(name) => {
                    let text = match command_help(&name) {
                        Some(h) => h,
                        None => format!("未知命令 `{}`。\n\n{}", name, help_text()),
                    };
                    handles.flow.platform.reply_text(message_id, &text).await?;
                }
            }
        }
        Command::Restart => {
            // Reply BEFORE exiting, then re-exec ourselves with the SAME
            // startup args and inherited stdio (so the log redirect to
            // test.log keeps working in the new process).
            handles
                .flow
                .platform
                .reply_text(message_id, "♻️ 正在重启，稍候…")
                .await?;
            // Remember where to announce the restart: the chat, plus the
            // command message/thread so an in-Topic command announces back
            // inside its topic.
            write_restart_notify(&handles.flow.layout, &RestartNotify::new(&thread_key, message_id));
            // Under a systemd unit, do NOT spawn a child: `KillMode=control-group`
            // would kill it when the unit stops. Exit with the supervisor-restart
            // code and let `Restart=on-failure` bring the unit back up from the
            // same ExecStart (ADR-0015, ADR-0021 — same as `/update`).
            if let Some(code) = crate::update::supervisor_restart_code() {
                // Hand back what this process holds before the unit brings the
                // replacement up (issue #918). Best effort and bounded: the
                // reply is already sent, and a wedged runtime must not hold the
                // restart.
                handles.flow.detach_for_exit().await;
                std::process::exit(code);
            }
            // The replacement spawns inside `restart_process`, so hand the
            // runtime back FIRST: a new connection that re-adopts a held thread
            // before this one lets go would keep it loaded, and the handback
            // would not start the grace window. A failed spawn leaves this
            // process running with its subscriptions released; the next read
            // re-adopts (a released thread is explicitly incomplete, never a
            // lost session).
            handles.flow.detach_for_exit().await;
            match restart_process() {
                Ok(()) => std::process::exit(0),
                Err(e) => {
                    tracing::error!("restart spawn failed: {}", e);
                    handles
                        .flow
                        .platform
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
            match handles.flow.backend.restart_server().await {
                Ok(crate::backend::RestartOutcome::Restarted) => {
                    handles
                        .flow
                        .platform
                        .reply_text(message_id, "♻️ 已重启 OpenCode 服务器。")
                        .await?;
                }
                Ok(crate::backend::RestartOutcome::RestartedService) => {
                    handles
                        .flow
                        .platform
                        .reply_text(
                            message_id,
                            "♻️ 已重启 OpenCode 托管服务（Shared Service）；cola 会在几秒内重新连接。",
                        )
                        .await?;
                }
                Ok(crate::backend::RestartOutcome::NotOwned) => {
                    handles
                        .flow
                        .platform
                        .reply_text(
                            message_id,
                            "这个 OpenCode 服务器不是 cola 启动的，需要你手动重启它。",
                        )
                        .await?;
                }
                Ok(crate::backend::RestartOutcome::NoServer) => {
                    handles
                        .flow
                        .platform
                        .reply_text(message_id, "当前没有正在运行的 OpenCode 服务器。")
                        .await?;
                }
                Err(e) => {
                    tracing::error!("restart opencode failed: {}", e);
                    handles
                        .flow
                        .platform
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
                feishu: &handles.flow.platform,
                message_id,
            };
            if let crate::update::UpdateOutcome::Updated(new_version) =
                crate::update::run_update(&reporter, crate::update::UpdateMode::Apply).await
            {
                handles.flow.platform.reply_text(message_id, "正在重启…").await?;
                let mut notify = RestartNotify::new(&thread_key, message_id);
                notify.kind = RestartKind::Update;
                notify.version = Some(new_version.to_string());
                write_restart_notify(&handles.flow.layout, &notify);
                // Same exit contract as `/restart` (issue #918): the new
                // process relearns its own attachments.
                handles.flow.detach_for_exit().await;
                crate::update::restart();
            }
        }
        Command::Version => {
            // Version identity (ADR-0027): a local text reply, never a network
            // call — it must work on any instance at any time.
            handles
                .flow
                .platform
                .reply_text(message_id, &crate::version::feishu_reply())
                .await?;
        }
        Command::Status => {
            // A text-only read (spec #922 decision 2): current + pending + a
            // compact session summary. The pending line is Codex's live
            // one-shot selection; a Pending Session shows its creation model
            // instead and never 待应用.
            let Some(settings) = handles.flow.sessions.session_settings(&thread_key).await else {
                handles
                    .flow
                    .platform
                    .reply_text(
                        message_id,
                        &format!(
                            "⚠️ {}还没有会话，先用 `/new` 或 `/dir` 创建。",
                            crate::bridge::display::feishu_side_label(&thread_key)
                        ),
                    )
                    .await?;
                return Ok(());
            };
            let at_level = |variant: Option<&str>| {
                variant
                    .filter(|variant| !variant.is_empty())
                    .map(|variant| format!("@{variant}"))
                    .unwrap_or_default()
            };
            let mut lines = Vec::new();
            match settings.session_id.as_deref() {
                None => {
                    let model = settings.model.as_deref().unwrap_or("未选择");
                    lines.push(format!(
                        "当前：{model}{}（将在会话创建时生效）",
                        at_level(settings.variant.as_deref())
                    ));
                }
                Some(session_id) => {
                    let current = handles
                        .flow
                        .sessions
                        .effective_selection(&handles.flow.backend, &settings)
                        .await;
                    let current_line = current
                        .as_ref()
                        .map(|model| {
                            format!(
                                "{}/{}{}",
                                model.provider_id,
                                model.id,
                                at_level(model.variant.as_deref())
                            )
                        })
                        .unwrap_or_else(|| "未知".to_string());
                    lines.push(format!("当前：{current_line}"));
                    if handles.flow.backend.owns_next_turn_pending()
                        && let Some(pending) = pending_label(&handles.flow.backend, session_id).await
                    {
                        lines.push(format!("待应用：{pending}（下一条新消息生效）"));
                    }
                }
            }
            let session = settings
                .session_id
                .as_deref()
                .map(|id| format!("会话：`{id}`"))
                .unwrap_or_else(|| "会话：待创建".to_string());
            let directory = if settings.directory.is_empty() {
                "(默认目录)".to_string()
            } else {
                format!("`{}`", settings.directory)
            };
            let agent = settings.agent.as_deref().unwrap_or("服务器默认");
            let auto_accept = if settings.auto_accept { "开" } else { "关" };
            lines.push(format!(
                "{session} · 目录：{directory} · Agent：{agent} · 自动通过：{auto_accept}"
            ));
            handles
                .flow
                .platform
                .reply_text(message_id, &lines.join("\n"))
                .await?;
        }
        Command::Forward(_) => {
            // Unreachable: the message coordinator intercepts `Command::Forward`
            // and routes it into the prompt pipeline itself (that pipeline is the
            // coordinator's job, and command dispatch must not depend on it).
        }
        Command::Skill(_) => {
            // Unreachable: like `Forward`, the message coordinator intercepts
            // `Command::Skill` and routes it into the prompt pipeline itself
            // (the skills must ride the prompt axis, which the coordinator owns).
        }
    }
    Ok(())
}

/// Dispatch a `/switch` action (ADR-0012). The text-direct forms share the
/// old command handlers; the no-arg form pops the interactive session card.
async fn handle_switch_action(
    handles: &CommandHandles,
    thread_key: &ThreadKey,
    action: SwitchAction,
    message_id: &str,
    kind: ConversationKind,
) -> crate::error::Result<()> {
    match action {
        SwitchAction::Card => {
            crate::feishu::card::command::send_switch_card(
                handles,
                thread_key,
                "",
                crate::feishu::card::session::SwitchScope::Directory,
                message_id,
            )
            .await?;
            Ok(())
        }
        SwitchAction::Match(keyword) => handle_switch(handles, thread_key, &keyword, message_id, kind).await,
        SwitchAction::Forget => {
            // Unmapping stops each removed Session from being this thread's
            // Active Session; the operation collects their waiting cards as
            // 「⏳ 已切换会话 · 后台任务仍在运行」 (ADR-0059, spec #405).
            let removed = handles.flow.unmap_thread(thread_key).await?;
            if removed.is_empty() {
                handles
                    .flow
                    .platform
                    .reply_text(message_id, "当前没有映射的会话。")
                    .await?;
            } else {
                handles
                    .flow
                    .platform
                    .reply_text(
                        message_id,
                        &format!(
                            "已解除{}的映射（服务器会话仍保留，可用 `/switch` 卡片或 `/switch <关键词>` 重新找到）。",
                            crate::bridge::display::feishu_side_label(thread_key)
                        ),
                    )
                    .await?;
            }
            Ok(())
        }
        SwitchAction::Attach { query, force } => {
            handle_attach(handles, thread_key, &query, force, message_id, kind).await
        }
    }
}

/// `/switch <keyword>` — switch within the thread first, then adopt a unique
/// global match (ADR-0008). Resolution order:
/// 1. Current thread's mapped sessions (matched by title/directory/id);
///    a unique hit switches without changing the mapping and confirms in one
///    text line carrying the status summary (ADR-0100).
/// 2. Global store search (sub-task children excluded); a unique hit adopts
///    into the current thread and becomes active.
/// 3. Multiple hits: list up to 8 candidates and point at `/attach`.
async fn handle_switch(
    handles: &CommandHandles,
    thread_key: &ThreadKey,
    keyword: &str,
    message_id: &str,
    kind: ConversationKind,
) -> crate::error::Result<()> {
    let sessions = handles
        .flow
        .sessions
        .cached_session_list(&handles.flow.backend)
        .await?;
    let lower = keyword.to_lowercase();

    // 1. Current thread's mapped sessions first.
    let thread_ids: Vec<String> = {
        let store = handles.flow.sessions.store.lock().await;
        store
            .list_thread(thread_key)
            .into_iter()
            .map(|e| e.session_id.clone())
            .collect()
    };
    let thread_hits: Vec<&crate::backend::types::SessionListInfo> = sessions
        .iter()
        .filter(|s| thread_ids.contains(&s.id) && matches_keyword(s, &lower))
        .collect();
    if thread_hits.len() == 1 {
        let hit = thread_hits[0];
        let entry = {
            let store = handles.flow.sessions.store.lock().await;
            store
                .list_thread(thread_key)
                .into_iter()
                .find(|e| e.session_id == hit.id)
                .cloned()
        };
        if let Some(entry) = entry {
            handles.flow.activate(entry).await?;
        }
        // ADR-0100: a re-activation confirms in ONE text line carrying the
        // status summary, whether or not there is content to report — the
        // snapshot card is retired from this path. Pending requests surface
        // through the ordinary sweep. The gather runs inside the Session's
        // `snapshot` span (ADR-0048).
        let line = crate::bridge::snapshot::adoption_line(&handles.snapshot_handles(), "切换", hit).await;
        handles.flow.platform.reply_text(message_id, &line).await?;
        return Ok(());
    }
    if thread_hits.len() > 1 {
        let list = candidates_list(
            &format!(
                "{}匹配到多个，请用 `/switch <完整ID>` 指定：",
                crate::bridge::display::feishu_side_label(thread_key)
            ),
            &thread_hits,
        );
        handles.flow.platform.reply_text(message_id, &list).await?;
        return Ok(());
    }

    // 2. Global search, children excluded.
    let global_hits: Vec<&crate::backend::types::SessionListInfo> = sessions
        .iter()
        .filter(|s| !s.is_child() && matches_keyword(s, &lower))
        .collect();
    if global_hits.len() == 1 {
        let hit = global_hits[0].clone();
        adopt_session(handles, thread_key, &hit, message_id, kind, false, "/switch").await?;
        return Ok(());
    }
    if global_hits.len() > 1 {
        let list = candidates_list("找到多个会话，请用 `/switch <完整ID>` 指定：", &global_hits);
        handles.flow.platform.reply_text(message_id, &list).await?;
        return Ok(());
    }
    // No match: send the interactive card pre-filtered by the keyword, so the
    // user sees the empty result AND can adjust the search / start fresh. The
    // text `/switch <kw>` searches the whole store (ADR-0022), so the card
    // opens in the `All` scope — a directory-scoped card would hide the
    // candidates the user was just shown.
    crate::feishu::card::command::send_switch_card(
        handles,
        thread_key,
        keyword,
        crate::feishu::card::session::SwitchScope::All,
        message_id,
    )
    .await?;
    Ok(())
}

/// Resolution outcome for a session keyword, shared by `/attach` and
/// `/topic --adopt` (ADR-0016). Exact id wins, then unique id match, then
/// unique title substring — the same order both commands documented.
enum SessionResolution<'a> {
    /// Exactly one session matched.
    Hit(&'a crate::backend::types::SessionListInfo),
    /// Several sessions matched; the caller should list candidates.
    Ambiguous(Vec<&'a crate::backend::types::SessionListInfo>),
    /// Nothing matched.
    None,
}

/// Resolve a session keyword against the shared store: exact id → unique id
/// match → unique title substring (shared by `/attach` and `/topic --adopt`).
/// The id match is tiered, highest precedence first: the full id prefix
/// (`ses_...`), the bare hash prefix the cards display (`id_tail` strips the
/// `ses_` prefix, so a copy-pasted card hash has no prefix), then a suffix of
/// the id's tail. A tier that matches decides before the next is tried, so a
/// prefix hit is never shadowed by a coincidental suffix hit. Child sessions
/// are NOT excluded here — exclusion is a per-command policy (ADR-0008,
/// ADR-0016).
fn resolve_session<'a>(
    sessions: &'a [crate::backend::types::SessionListInfo],
    query: &str,
) -> SessionResolution<'a> {
    let lower = query.to_lowercase();
    if let Some(s) = sessions.iter().find(|s| s.id == query) {
        return SessionResolution::Hit(s);
    }
    let full_prefix: Vec<&crate::backend::types::SessionListInfo> = sessions
        .iter()
        .filter(|s| s.id.to_lowercase().starts_with(&lower))
        .collect();
    if let Some(r) = decide_id_hits(full_prefix) {
        return r;
    }
    let bare_prefix: Vec<&crate::backend::types::SessionListInfo> = sessions
        .iter()
        .filter(|s| {
            let id = s.id.to_lowercase();
            id.strip_prefix("ses_").unwrap_or(&id).starts_with(&lower)
        })
        .collect();
    if let Some(r) = decide_id_hits(bare_prefix) {
        return r;
    }
    let suffix: Vec<&crate::backend::types::SessionListInfo> = sessions
        .iter()
        .filter(|s| {
            let id = s.id.to_lowercase();
            id.strip_prefix("ses_").unwrap_or(&id).ends_with(&lower)
        })
        .collect();
    if let Some(r) = decide_id_hits(suffix) {
        return r;
    }
    let titles: Vec<&crate::backend::types::SessionListInfo> = sessions
        .iter()
        .filter(|s| s.title.to_lowercase().contains(&lower))
        .collect();
    if titles.len() == 1 {
        return SessionResolution::Hit(titles[0]);
    }
    if titles.len() > 1 {
        return SessionResolution::Ambiguous(titles);
    }
    SessionResolution::None
}

/// Turn one id-match tier into a resolution, or `None` when the tier is empty
/// (so the caller falls through to the next, lower-precedence tier).
fn decide_id_hits<'a>(
    hits: Vec<&'a crate::backend::types::SessionListInfo>,
) -> Option<SessionResolution<'a>> {
    match hits.len() {
        0 => None,
        1 => Some(SessionResolution::Hit(hits[0])),
        _ => Some(SessionResolution::Ambiguous(hits)),
    }
}

/// The child-adoption policy of the general session commands (spec #344): a
/// child session is only ever taken over through `/sub attach`, so the
/// `/switch ... --force` and `/switch` card adoption paths refuse one that is
/// not already mapped to this chat — `force` never opens this door. A child
/// this chat mapped itself is a normal switch target (re-activation, or a
/// `--force` steal from a sibling thread) and passes. The refusal names the
/// sanctioned command; it lives here, at the command layer, so the shared
/// [`resolve_session`] stays a neutral resolver.
pub(crate) fn child_adoption_refusal(
    store: &crate::bridge::session::SessionStore,
    thread_key: &ThreadKey,
    info: &crate::backend::types::SessionListInfo,
) -> Option<String> {
    if !info.is_child() {
        return None;
    }
    let mapped_to_chat = store
        .thread_for_session(&info.id)
        .is_some_and(|owner| owner.chat_id == thread_key.chat_id);
    if mapped_to_chat {
        return None;
    }
    Some(format!(
        "⚠️ 会话「{}」是子任务会话，不能用 `/switch` 接管；请用 `/sub attach {}` 接管它（该命令只接管当前会话的直接子会话）。",
        crate::bridge::display::title_or_id_tail(info),
        info.id
    ))
}

/// `/attach <id|title> [--force]` — take over an arbitrary ROOT server session
/// into the current thread (ADR-0008). Resolution: exact id → unique
/// id-prefix → unique title substring; multiple hits list candidates. A child
/// is refused unless this chat already mapped it — the takeover path for
/// children is `/sub attach` (spec #344).
async fn handle_attach(
    handles: &CommandHandles,
    thread_key: &ThreadKey,
    query: &str,
    force: bool,
    message_id: &str,
    kind: ConversationKind,
) -> crate::error::Result<()> {
    let sessions = handles
        .flow
        .sessions
        .cached_session_list(&handles.flow.backend)
        .await?;
    match resolve_session(&sessions, query) {
        SessionResolution::Hit(s) => {
            let refusal = {
                let store = handles.flow.sessions.store.lock().await;
                child_adoption_refusal(&store, thread_key, s)
            };
            if let Some(refusal) = refusal {
                handles.flow.platform.reply_text(message_id, &refusal).await?;
                return Ok(());
            }
            adopt_session(handles, thread_key, s, message_id, kind, force, "/switch").await
        }
        SessionResolution::Ambiguous(hits) => {
            let list = candidates_list("找到多个会话，请用完整 ID：", &hits);
            handles.flow.platform.reply_text(message_id, &list).await?;
            Ok(())
        }
        SessionResolution::None => {
            handles
                .flow
                .platform
                .reply_text(message_id, &format!("No session matching \"{}\"", query))
                .await?;
            Ok(())
        }
    }
}

/// `/sub attach <id|id-prefix|title> [--force]` — take over one of the Active
/// Session's DIRECT children into the current chat (spec #344). The query
/// resolves among those children with the `/switch` rules (exact id → unique
/// id-prefix → title substring); a session outside that scope is refused, never
/// adopted — the child filter is this command's policy, not the shared
/// resolver's (spec #344). Adoption then reuses [`adopt_session`] end to end:
/// Session Mapping activation, the ONE confirmation text line carrying the
/// child's status (ADR-0100), the owner check, and `--force` to steal a mapping
/// owned by another chat. The parent stays mapped and switchable; no prompt is
/// ever sent into the child (ADR-0054).
async fn handle_sub_attach(
    handles: &CommandHandles,
    thread_key: &ThreadKey,
    query: &str,
    force: bool,
    message_id: &str,
    kind: ConversationKind,
) -> crate::error::Result<()> {
    let Some(active_id) = handles.flow.sessions.get_session_id(thread_key).await else {
        handles
            .flow
            .platform
            .reply_text(
                message_id,
                &format!(
                    "⚠️ {}没有活动会话，`/sub attach` 需要当前会话的直接子会话。先用 `/new`、`/dir` 或 `/switch` 建立会话。",
                    crate::bridge::display::feishu_side_label(thread_key)
                ),
            )
            .await?;
        return Ok(());
    };
    let sessions = handles
        .flow
        .sessions
        .cached_session_list(&handles.flow.backend)
        .await?;
    let children: Vec<crate::backend::types::SessionListInfo> = sessions
        .iter()
        .filter(|s| s.is_child_of(&active_id) && !s.is_archived())
        .cloned()
        .collect();
    match resolve_session(&children, query) {
        SessionResolution::Hit(s) => {
            adopt_session(handles, thread_key, s, message_id, kind, force, "/sub attach").await
        }
        SessionResolution::Ambiguous(hits) => {
            let list = candidates_list("找到多个子会话，请用完整 ID：", &hits);
            handles.flow.platform.reply_text(message_id, &list).await?;
            Ok(())
        }
        SessionResolution::None => {
            // The already-active child can never appear in its own children
            // list, so re-running `/sub attach` on it must be caught here. The
            // fallback is ONLY for a child the server reports: an active ROOT
            // named by the query is a non-child and must fall through to the
            // scope refusal, and a stale mapping absent from the fetched list
            // is not in scope either — fabricating a record would bypass the
            // scope gate. It goes through the same adoption path, whose
            // already-active branch writes nothing and sends no card.
            if let Some(active_info) = sessions.iter().find(|s| s.id == active_id && s.is_child())
                && matches!(
                    resolve_session(std::slice::from_ref(active_info), query),
                    SessionResolution::Hit(_)
                )
            {
                return adopt_session(
                    handles,
                    thread_key,
                    active_info,
                    message_id,
                    kind,
                    force,
                    "/sub attach",
                )
                .await;
            }
            // Not a direct child: resolve against the rest of the store to tell
            // a scope refusal apart from a plain no-match. Archived sessions
            // (children included) are no more adoptable than they are listable,
            // so they read as no-match.
            let out_of_scope: Vec<crate::backend::types::SessionListInfo> = sessions
                .iter()
                .filter(|s| !s.is_child_of(&active_id) && !s.is_archived())
                .cloned()
                .collect();
            match resolve_session(&out_of_scope, query) {
                SessionResolution::Hit(s) => {
                    handles.flow.platform
                        .reply_text(
                            message_id,
                            &format!(
                                "⚠️ 会话「{}」不是当前会话的直接子会话，`/sub attach` 只能接管当前会话的直接子会话。",
                                crate::bridge::display::title_or_id_tail(s)
                            ),
                        )
                        .await?;
                    Ok(())
                }
                SessionResolution::Ambiguous(hits) => {
                    let list = candidates_list("⚠️ 匹配到的会话都不是当前会话的直接子会话：", &hits);
                    handles.flow.platform.reply_text(message_id, &list).await?;
                    Ok(())
                }
                SessionResolution::None => {
                    handles
                        .flow
                        .platform
                        .reply_text(message_id, &format!("没有匹配的子会话：\"{}\"", query))
                        .await?;
                    Ok(())
                }
            }
        }
    }
}

/// `/topic --adopt <keyword> [--force]` (ADR-0016): open a real Feishu topic
/// whose backing session is an EXISTING server session, in one gesture.
/// Resolves the session (`resolve_session`), rejects child sessions, honors
/// `--force` for a session mapped to another thread, then creates the topic
/// via `reply_in_thread` on the command message and maps the adopted session
/// to the NEW topic's `ThreadKey` (anchor = the in-topic confirmation).
async fn handle_topic_adopt(
    handles: &CommandHandles,
    thread_key: &ThreadKey,
    keyword: &str,
    force: bool,
    message_id: &str,
) -> crate::error::Result<()> {
    let sessions = handles
        .flow
        .sessions
        .cached_session_list(&handles.flow.backend)
        .await?;
    let info = match resolve_session(&sessions, keyword) {
        SessionResolution::Hit(s) => s.clone(),
        SessionResolution::Ambiguous(hits) => {
            let list = candidates_list("找到多个会话，请用完整 ID：", &hits);
            handles.flow.platform.reply_text(message_id, &list).await?;
            return Ok(());
        }
        SessionResolution::None => {
            handles
                .flow
                .platform
                .reply_text(message_id, &format!("No session matching \"{}\"", keyword))
                .await?;
            return Ok(());
        }
    };
    // Child sessions (sub-task contexts) are never adoptable (ADR-0016): the
    // server allows POSTing to them, but OpenChamber does not either and the
    // task-derived temporary context is meaningless to drive.
    if info.is_child() {
        handles
            .flow
            .platform
            .reply_text(
                message_id,
                &format!("⚠️ 会话 `{}` 是子任务会话，不支持接管。", info.title),
            )
            .await?;
        return Ok(());
    }
    // Mapped to another thread: reject unless --force (mirrors adopt_session).
    // With `--force` the steal rides the topic's own activation below:
    // `open_topic` opens and seeds the topic FIRST, then its atomic activation
    // replaces the old owner's mapping — so a failed open/send or a refused
    // save leaves the old owner's mapping (and its waiting card) untouched.
    // Unmapping here would lose it before the topic existed.
    let owner = {
        let store = handles.flow.sessions.store.lock().await;
        store.thread_for_session(&info.id)
    };
    if let Some(owner_key) = owner
        && owner_key != *thread_key
        && !force
    {
        let chat_name = handles
            .flow
            .platform
            .chat_name(&owner_key.chat_id)
            .await
            .unwrap_or(None)
            .unwrap_or_else(|| owner_key.chat_id.clone());
        let where_flag = if owner_key.thread_id != owner_key.chat_id {
            "话题"
        } else {
            "主对话"
        };
        handles.flow.platform
                .reply_text(
                    message_id,
                    &format!(
                        "⚠️ 会话 `{}`（目录 `{}`）已被其他聊天占用：\n{}\n（{}，chat `{}`）\n\n可先请对方 `/switch forget` 解除，或使用 `/topic --adopt {} --force` 强行接管。",
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
    // Open the topic in one transaction (ADR-0016): the pre-mapping
    // confirmation read, cover card (ADR-0023), in-topic text seed and Session
    // Mapping all live in `bridge::topic`. The adopted session's pending
    // requests are left to the ordinary sweep (ADR-0100).
    match crate::bridge::topic::open_topic(
        &handles.topic_handles(),
        &thread_key.chat_id,
        message_id,
        crate::bridge::topic::TopicOpening::Adopt { info },
    )
    .await
    {
        Ok(opened) => {
            tracing::info!(
                "topic-adopt: created topic {} for adopted session {} in chat {}",
                opened.thread_id,
                opened.session_id.as_deref().unwrap_or("?"),
                thread_key.chat_id
            );
            Ok(())
        }
        Err(crate::bridge::topic::OpenTopicError::NoThreadId) => {
            // No thread_id from the platform — report and point at the fallback.
            handles
                .flow
                .platform
                .reply_text(
                    message_id,
                    "⚠️ 当前会话不支持创建话题（未返回 thread_id）。请改用 `/switch <id>` 接管到当前会话。",
                )
                .await?;
            Ok(())
        }
        Err(crate::bridge::topic::OpenTopicError::Failed(e)) => Err(e),
    }
}

/// Adopt a server session as the current thread's session, honoring the
/// one-session-one-thread invariant (ADR-0007). Already the active session
/// → idempotent no-op. Mapped to another thread → rejected with an
/// actionable card unless `--force` (which steals the mapping). Copies
/// `directory` + `agent` from the server; `auto_accept` resets to false.
///
/// ADR-0100: the adoption confirmation is ONE text line carrying the status
/// summary the Session Snapshot used to show — no card. In a topic it is
/// replied in-thread and its message id becomes the fallback-card anchor
/// (`reply_in_thread`, ADR-0006), so permission/question cards still land
/// inside the topic; in the lobby it is a plain reply. Pending requests surface
/// through the ordinary sweep.
///
/// `force_hint` is the command the owner refusal points the user at (with
/// `--force` appended): `/switch` for the general adoption path, `/sub attach`
/// for the child-takeover path, so a refusal never advertises a route the
/// caller's own policy would refuse.
async fn adopt_session(
    handles: &CommandHandles,
    thread_key: &ThreadKey,
    info: &crate::backend::types::SessionListInfo,
    message_id: &str,
    kind: ConversationKind,
    force: bool,
    force_hint: &str,
) -> crate::error::Result<()> {
    // Idempotent: already the active session of this thread.
    {
        let store = handles.flow.sessions.store.lock().await;
        if let Some(e) = store.get_active(thread_key)
            && e.session_id == info.id
        {
            handles
                .flow
                .platform
                .reply_text(message_id, &format!("Already active: \"{}\".", info.title))
                .await?;
            return Ok(());
        }
    }
    // Mapped to another thread: reject unless --force.
    let owner = {
        let store = handles.flow.sessions.store.lock().await;
        store.thread_for_session(&info.id)
    };
    if let Some(owner_key) = owner.as_ref()
        && owner_key != thread_key
        && !force
    {
        let chat_name = handles
            .flow
            .platform
            .chat_name(&owner_key.chat_id)
            .await
            .unwrap_or(None)
            .unwrap_or_else(|| owner_key.chat_id.clone());
        let where_flag = if owner_key.thread_id != owner_key.chat_id {
            "话题"
        } else {
            "主对话"
        };
        handles.flow.platform
                .reply_text(
                    message_id,
                    &format!(
                        "⚠️ 会话 `{}`（目录 `{}`）已被其他聊天占用：\n{}\n（{}，chat `{}`）\n\n可先请对方 `/switch forget` 解除，或使用 `{} {} --force` 强行接管。",
                        info.title,
                        info.directory,
                        chat_name,
                        where_flag,
                        owner_key.chat_id,
                        force_hint,
                        info.id
                    ),
                )
                .await?;
        return Ok(());
    }

    // ADR-0100: the confirmation is ONE text line carrying the status summary
    // (gathered before the mapping write below, so it reflects the session's
    // pre-adoption state; each read field is best-effort). Pending requests are
    // left to the ordinary sweep — nothing claims them here.
    let line = crate::bridge::snapshot::adoption_line(&handles.snapshot_handles(), "接管", info).await;

    // Send the confirmation BEFORE any store mutation. A failed send then
    // leaves the previous owner's mapping (and its waiting card) intact, so
    // the user can simply retry; the old ordering unmapped first and could
    // strand the Session when a topic send failed.
    //
    // In a topic the line doubles as the fallback-card anchor (ADR-0023): its
    // message id becomes `topic_anchor`, so permission/question cards land
    // inside the topic.
    let anchor = if kind == ConversationKind::Topic {
        Some(handles.flow.platform.reply_in_thread(message_id, &line).await?.0)
    } else {
        handles.flow.platform.reply_text(message_id, &line).await?;
        None
    };

    // The activation below IS the steal: its atomic store write replaces any
    // existing mapping of the session id, so a `--force` adoption moves the
    // Session here in the same write that drops the old owner. Nothing is
    // unmapped ahead of it, and `FlowHandles::activate` collects both displaced
    // waiting cards only after that write persisted (ADR-0059, spec #405).
    //
    // ADR-0041: when this topic's session was still pending (the `/switch <id>`
    // recovery path for a mistaken `/topic`), carry its cover root onto the
    // adopted entry — the quote-injection guard and the cover record must not
    // be lost with the pending.
    let pending_topic_root = {
        let store = handles.flow.sessions.store.lock().await;
        store.pending_for(thread_key).and_then(|p| p.topic_root.clone())
    };
    let mut entry = SessionEntry::new(thread_key.clone(), info.id.clone(), info.directory.clone());
    entry.agent = info.agent.clone();
    entry.topic_anchor = anchor;
    entry.topic_root = pending_topic_root;
    if let Err(e) = handles.flow.activate(entry).await {
        // One atomic write: a failure leaves the previous owner's mapping AND
        // its waiting card exactly as they were, so the notice can promise both.
        // The confirmation line already said the Session was taken over, so a
        // silent error would leave the user believing a mapping that was never
        // written. Say so in the chat (best effort), then still return the
        // error for the caller's log.
        report_adoption_failure(handles, message_id, &info.title, &e).await;
        return Err(e);
    }
    crate::bridge::topic::claim_pending_cover(&handles.topic_handles(), thread_key, &info.id).await;
    Ok(())
}

/// Report a failed adoption store write in the chat.
///
/// The confirmation line is sent **before** the mapping write, so a silent
/// failure would leave the user believing the Session moved. The store's
/// activation is atomic and the displaced card is collected only after it
/// persists, so the previous mapping and its card are guaranteed unchanged.
async fn report_adoption_failure(
    handles: &CommandHandles,
    message_id: &str,
    title: &str,
    error: &crate::error::BridgeError,
) {
    let _ = handles
        .flow
        .platform
        .reply_text(
            message_id,
            &format!("⚠️ 会话 `{title}` 接管失败：{error}。原映射保持不变，可稍后重试。"),
        )
        .await;
}

/// Normalize a user-supplied directory for `/dir` / `/topic` into an absolute
/// path the OpenCode server can route by: expand a leading `~`, resolve
/// relative paths against the working directory, and canonicalize (`..`,
/// symlinks) when the path exists. OpenCode sessions are keyed by their
/// directory and the server fails on a session created with a `~`-style or
/// relative path, so cola must hand it a real absolute directory.
pub(crate) fn normalize_directory(input: &str) -> std::path::PathBuf {
    let expanded = expand_directory(input);
    // Canonicalize (resolving `..` and symlinks) when the directory exists;
    // otherwise keep the normalized absolute form — the caller checks
    // `is_dir()` and reports it as missing.
    std::fs::canonicalize(&expanded).unwrap_or(expanded)
}

/// Expand a user-supplied directory to an absolute path WITHOUT canonicalizing:
/// a leading `~` resolves against home, a relative path against the working
/// directory. The `/dir` browse card (ADR-0078) uses this so the browsed
/// spelling — and any symlink in it — stays as the user navigated it; the
/// canonicalizing [`normalize_directory`] stays the pre-session form.
pub(crate) fn expand_directory(input: &str) -> std::path::PathBuf {
    expand_against(input, &std::env::current_dir().unwrap_or_default())
}

/// [`expand_directory`] with an explicit base for a relative path: `~` still
/// expands against home, an absolute path still passes through, and a relative
/// one joins `base` instead of the process cwd. The **Path Scope** (ADR-0083
/// §4) is the one caller that needs it — a `/cat`/viewer path resolves against
/// the Session's directory (the conversation's current project while the
/// Session is still Pending), never against wherever cola was started.
pub(crate) fn expand_against(input: &str, base: &std::path::Path) -> std::path::PathBuf {
    let home = dirs::home_dir().unwrap_or_default();
    let expanded = if input == "~" {
        home.clone()
    } else if let Some(rest) = input.strip_prefix("~/") {
        home.join(rest)
    } else {
        std::path::PathBuf::from(input)
    };
    if expanded.is_absolute() {
        expanded
    } else {
        base.join(expanded)
    }
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
/// (used by `/switch <keyword>` and the `/switch` card's filtered list).
/// `/attach` and `/topic --adopt` do NOT use this —
/// they resolve via `resolve_session` (exact id → id-prefix → whole-title
/// substring) so adoption stays unambiguous.
///
/// `lower` is split on whitespace (spaces AND newlines) and every token must
/// hit somewhere in the session's title/dir/id — an AND over tokens. This makes
/// the multiline `/switch` search box usable: pasted/typed multi-line input is
/// searched as its words, not as one literal string (which could never match).
pub(crate) fn matches_keyword(s: &crate::backend::types::SessionListInfo, lower: &str) -> bool {
    let haystack = format!(
        "{} {} {}",
        s.title.to_lowercase(),
        s.directory.to_lowercase(),
        s.id.to_lowercase()
    );
    token_and_match(&haystack, lower)
}

/// The search cards' one match rule (ADR-0051): `lower_keyword` is already
/// lowercased, split on whitespace, and every token must appear as a substring
/// of `haystack` (which the caller lowercases). Shared by the `/switch` and
/// `/skill` session match, the Recent Directories path match, and the browse
/// card's child-name filter, so the three cannot drift.
pub(crate) fn token_and_match(haystack: &str, lower_keyword: &str) -> bool {
    lower_keyword.split_whitespace().all(|tok| haystack.contains(tok))
}

/// Collapse `.` and `..` **textually** — no filesystem access, no symlink
/// resolution: `/a/b/../../c` → `/c`. The `/dir` browse card's path resolution
/// uses this on the tail of a not-yet-existing target, after canonicalizing the
/// longest existing prefix, so `..` is applied to a symlink-free base.
pub(crate) fn lexical_normalize(path: &std::path::Path) -> std::path::PathBuf {
    use std::path::Component;
    let mut out = std::path::PathBuf::new();
    for component in path.components() {
        match component {
            // `pop` failing means an empty (or root) base: keep a relative
            // `..`, never climb above an absolute root.
            Component::ParentDir => {
                if !out.pop() && !out.has_root() {
                    out.push("..");
                }
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Whether two directory spellings name the same directory: equal strings, or
/// canonicalize to the same path (best-effort — an unresolvable path compares by
/// string only). The one rule the browse card's `当前` mark and its pick share
/// (ADR-0078), like the search cards' `token_and_match`.
pub(crate) fn same_dir(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    matches!(
        (std::fs::canonicalize(a), std::fs::canonicalize(b)),
        (Ok(x), Ok(y)) if x == y
    )
}

/// The "ambiguous match" candidate list shown when a keyword or id resolves to
/// several sessions: `title · dir · id-tail`, capped at 8.
fn candidates_list(header: &str, sessions: &[&crate::backend::types::SessionListInfo]) -> String {
    let mut list = String::from(header);
    for s in sessions.iter().take(8) {
        list.push_str(&format!("- {} · {} · {}\n", s.title, s.directory, id_tail(&s.id)));
    }
    list
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_is_not_command() {
        assert_eq!(parse_command("hello world"), None);
        assert_eq!(parse_command("fix the bug"), None);
    }

    fn session(id: &str, title: &str) -> crate::backend::types::SessionListInfo {
        crate::backend::types::SessionListInfo {
            id: id.into(),
            title: title.into(),
            directory: "/work/x".into(),
            parent_id: None,
            agent: None,
            model: None,
            time: None,
        }
    }

    /// The hash the cards display (`id_tail`) is the id without its `ses_`
    /// prefix, so a copy-pasted card hash must resolve — not just a full-id
    /// prefix. This is the `/switch <hash> --force` path (issue: the toast
    /// pointed at a command whose ID the user could not obtain).
    #[test]
    fn resolve_session_accepts_the_bare_card_hash() {
        let sessions = vec![
            session("ses_1a2b3c4d5e6f7g8h9i0j", "A"),
            session("ses_9z8y7x6w5v4u3t2s1r0q", "B"),
        ];
        let shown = id_tail(&sessions[0].id);
        assert_eq!(shown, "1a2b3c4");
        match resolve_session(&sessions, &shown) {
            SessionResolution::Hit(s) => assert_eq!(s.id, sessions[0].id),
            _ => panic!("the displayed hash must resolve"),
        }
    }

    /// Full-id prefix and id-tail suffix both resolve (users quote either end).
    #[test]
    fn resolve_session_accepts_prefix_and_suffix() {
        let sessions = vec![session("ses_1a2b3c4d5e6f7g8h9i0j", "A")];
        for q in ["ses_1a2b", "1a2b3c4", "h9i0j"] {
            match resolve_session(&sessions, q) {
                SessionResolution::Hit(s) => assert_eq!(s.id, sessions[0].id, "query {q}"),
                _ => panic!("query {q} must resolve"),
            }
        }
    }

    /// Two sessions sharing the displayed hash (a time-prefix collision) list
    /// candidates instead of silently picking one.
    #[test]
    fn resolve_session_reports_ambiguous_hashes() {
        let sessions = vec![session("ses_1a2b3c4aaaa", "A"), session("ses_1a2b3c4bbbb", "B")];
        assert!(matches!(
            resolve_session(&sessions, "1a2b3c4"),
            SessionResolution::Ambiguous(_)
        ));
    }

    /// A prefix hit outranks a coincidental suffix hit: the id-match tiers
    /// decide in order, so the lower tier never makes the higher one ambiguous.
    #[test]
    fn resolve_session_prefers_prefix_over_suffix() {
        let sessions = vec![
            session("ses_1a2bXxxxx", "prefix"),
            session("ses_zzzz1a2b", "suffix"),
        ];
        match resolve_session(&sessions, "1a2b") {
            SessionResolution::Hit(s) => assert_eq!(s.id, "ses_1a2bXxxxx"),
            _ => panic!("the prefix hit must win"),
        }
    }

    #[test]
    fn parse_dir_with_path() {
        let cmd = parse_command("/dir /home/user/project");
        assert_eq!(cmd, Some(Command::Dir("/home/user/project".into())));
    }

    #[test]
    fn parse_dir_missing_arg_is_dir_card() {
        assert_eq!(parse_command("/dir"), Some(Command::DirCard));
        assert_eq!(parse_command("/name"), Some(Command::Help(Some("name".into()))));
    }

    #[test]
    fn parse_agent_model_no_arg_is_card() {
        assert_eq!(parse_command("/model"), Some(Command::ModelCard));
        assert_eq!(parse_command("/status"), Some(Command::Status));
        assert_eq!(parse_command("/agent"), Some(Command::AgentCard));
    }

    #[test]
    fn parse_think_no_arg_is_card_with_arg_is_value() {
        assert_eq!(parse_command("/think"), Some(Command::ThinkCard));
        assert_eq!(parse_command("/think high"), Some(Command::Think("high".into())));
        assert_eq!(
            parse_command("/think default"),
            Some(Command::Think("default".into()))
        );
        assert_eq!(
            parse_command("/think --reset"),
            Some(Command::Think("--reset".into()))
        );
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

    /// The retired text-list subcommand is gone: its first word is an ordinary
    /// keyword, so it parses exactly like any other `/switch <keyword>` and the
    /// bridge resolves "list" against the session store (no dedicated reply).
    #[test]
    fn parse_switch_list_is_an_ordinary_keyword() {
        assert_eq!(
            parse_command("/switch list"),
            Some(Command::Switch(SwitchAction::Match("list".into())))
        );
        assert_eq!(
            parse_command(" /switch list "),
            Some(Command::Switch(SwitchAction::Match("list".into())))
        );
    }

    /// The retired `--all` flag has no parser meaning: it stays in the keyword
    /// like any other word.
    #[test]
    fn parse_switch_list_args_are_ordinary_keywords() {
        assert_eq!(
            parse_command("/switch list cola"),
            Some(Command::Switch(SwitchAction::Match("list cola".into())))
        );
        assert_eq!(
            parse_command("/switch list --all"),
            Some(Command::Switch(SwitchAction::Match("list --all".into())))
        );
        assert_eq!(
            parse_command("/switch list cola --all"),
            Some(Command::Switch(SwitchAction::Match("list cola --all".into())))
        );
        assert_eq!(
            parse_command("/switch list multi word"),
            Some(Command::Switch(SwitchAction::Match("list multi word".into())))
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

    /// Spec #344: `/sub`, `/sub list` and `/sub list <keyword>` are the whole
    /// grammar. An unknown verb (a bare keyword, or `attach` until #348 claims
    /// it) answers with the `/sub` help topic instead of being read as a
    /// keyword.
    #[test]
    fn parse_sub_opens_the_child_card() {
        assert_eq!(
            parse_command("/sub"),
            Some(Command::Sub(SubAction::List(String::new())))
        );
        assert_eq!(
            parse_command(" /sub "),
            Some(Command::Sub(SubAction::List(String::new())))
        );
        assert_eq!(
            parse_command("/sub list"),
            Some(Command::Sub(SubAction::List(String::new())))
        );
        assert_eq!(
            parse_command("/sub list 渲染"),
            Some(Command::Sub(SubAction::List("渲染".into())))
        );
        assert_eq!(
            parse_command("/sub list multi word"),
            Some(Command::Sub(SubAction::List("multi word".into())))
        );
        // The sanctioned grammar has no bare-keyword form.
        assert_eq!(
            parse_command("/sub 渲染"),
            Some(Command::Help(Some("sub".into())))
        );
    }

    /// Spec #344: `/sub attach <query> [--force]` is the takeover form. The
    /// query follows the `/switch` id forms; a bare `attach` is not a form, so
    /// it gets the help topic instead of an empty-query adoption.
    #[test]
    fn parse_sub_attach_takes_a_query_and_force() {
        assert_eq!(
            parse_command("/sub attach ses_abc123"),
            Some(Command::Sub(SubAction::Attach {
                query: "ses_abc123".into(),
                force: false
            }))
        );
        assert_eq!(
            parse_command("/sub attach ses_abc123 --force"),
            Some(Command::Sub(SubAction::Attach {
                query: "ses_abc123".into(),
                force: true
            }))
        );
        // `--force` is a flag wherever it appears; the rest is the query.
        assert_eq!(
            parse_command("/sub attach --force 重写 渲染"),
            Some(Command::Sub(SubAction::Attach {
                query: "重写 渲染".into(),
                force: true
            }))
        );
        // A bare `attach` (with or without the flag) is not a form.
        assert_eq!(
            parse_command("/sub attach"),
            Some(Command::Help(Some("sub".into())))
        );
        assert_eq!(
            parse_command("/sub attach --force"),
            Some(Command::Help(Some("sub".into())))
        );
    }

    #[test]
    fn parse_restart() {
        assert_eq!(parse_command("/restart"), Some(Command::Restart));
        assert_eq!(parse_command("/restart now"), Some(Command::Restart));
        assert_eq!(parse_command("/update"), Some(Command::Update));
        assert_eq!(parse_command("/update now"), Some(Command::Update));
        assert_eq!(parse_command("/version"), Some(Command::Version));
        assert_eq!(parse_command("/version x"), Some(Command::Version));
    }

    /// The restart announce payload carries the command message and thread so
    /// an in-Topic `/restart` can be announced back inside its topic.
    #[test]
    fn restart_notify_payload_carries_topic_reply_target() {
        let topic = ThreadKey::new("oc_1".into(), "omt_1".into());
        let notify = RestartNotify::new(&topic, "om_cmd");
        assert_eq!(notify.chat_id, "oc_1");
        assert_eq!(notify.thread_id.as_deref(), Some("omt_1"));
        assert_eq!(notify.message_id.as_deref(), Some("om_cmd"));
        assert_eq!(notify.kind, RestartKind::Restart);
        assert_eq!(notify.version, None);
    }

    /// A payload written by an older cola carries only `chat_id`: it must still
    /// deserialize and announce in the chat lobby (no in-topic reply target).
    #[test]
    fn old_restart_notify_payload_deserializes() {
        let notify: RestartNotify =
            serde_json::from_str(r#"{"chat_id":"oc_1"}"#).expect("old payload parses");
        assert_eq!(notify.chat_id, "oc_1");
        assert_eq!(notify.message_id, None);
        assert_eq!(notify.thread_id, None);
        assert_eq!(notify.kind, RestartKind::Restart);
        assert_eq!(notify.version, None);
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
                directory: Some("/root/proj/lib".into()),
                name: Some("api-refactor".into()),
            })
        );
    }

    #[test]
    fn parse_topic_with_dir_only() {
        assert_eq!(
            parse_command("/topic /root/proj/lib"),
            Some(Command::Topic {
                directory: Some("/root/proj/lib".into()),
                name: None,
            })
        );
    }

    /// Bare `/topic` inherits the current project (directory: None), resolved
    /// at command time — it no longer shows help.
    #[test]
    fn parse_topic_bare_uses_current_project() {
        assert_eq!(
            parse_command("/topic"),
            Some(Command::Topic {
                directory: None,
                name: None,
            })
        );
        assert_eq!(
            parse_command("/topic "),
            Some(Command::Topic {
                directory: None,
                name: None,
            })
        );
    }

    #[test]
    fn parse_topic_adopt_keyword() {
        assert_eq!(
            parse_command("/topic --adopt 重写登录模块"),
            Some(Command::TopicAdopt {
                keyword: "重写登录模块".into(),
                force: false,
            })
        );
    }

    #[test]
    fn parse_topic_adopt_force() {
        assert_eq!(
            parse_command("/topic --adopt ses_abc123 --force"),
            Some(Command::TopicAdopt {
                keyword: "ses_abc123".into(),
                force: true,
            })
        );
    }

    #[test]
    fn parse_topic_adopt_no_keyword_is_card() {
        assert_eq!(parse_command("/topic --adopt"), Some(Command::TopicAdoptCard));
        assert_eq!(parse_command("/topic --adopt  "), Some(Command::TopicAdoptCard));
    }

    #[test]
    fn parse_topic_adopt_multi_word_title() {
        assert_eq!(
            parse_command("/topic --adopt rewrite login module"),
            Some(Command::TopicAdopt {
                keyword: "rewrite login module".into(),
                force: false,
            })
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
    fn parse_card() {
        assert_eq!(parse_command("/card"), Some(Command::Card));
        // Like `/stop`/`/version`, a stray argument is ignored.
        assert_eq!(parse_command("/card now"), Some(Command::Card));
        assert_eq!(parse_command("/CARD"), Some(Command::Card));
    }

    /// `/cat` is a registered Command (ADR-0083 §2): a path opens it, a bare
    /// `/cat` answers usage, the token is case-insensitive and takes the whole
    /// rest of the line, and a longer token stays an unknown command — so
    /// `/cat` swallows nothing else.
    #[test]
    fn parse_cat() {
        assert_eq!(
            parse_command("/cat src/main.rs"),
            Some(Command::Cat(Some("src/main.rs".into())))
        );
        assert_eq!(parse_command("/cat"), Some(Command::Cat(None)));
        assert_eq!(parse_command("/cat   "), Some(Command::Cat(None)));
        assert_eq!(
            parse_command("/CAT ~/notes.md"),
            Some(Command::Cat(Some("~/notes.md".into())))
        );
        assert_eq!(
            parse_command("/cat /tmp/a b.txt"),
            Some(Command::Cat(Some("/tmp/a b.txt".into()))),
            "the whole remainder is the path"
        );
        // Only the exact token is `/cat`: unknown `/...` commands still forward.
        assert_eq!(
            parse_command("/catalog x"),
            Some(Command::Forward("/catalog x".into()))
        );
        assert_eq!(parse_command("/cats"), Some(Command::Forward("/cats".into())));
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

    /// `/diff` has exactly two forms: no argument, or `--base <ref>` with a
    /// value that is not itself a flag. Everything else is the usage variant,
    /// which runs no git.
    #[test]
    fn parse_diff() {
        assert_eq!(parse_command("/diff"), Some(Command::Diff(None)));
        assert_eq!(parse_command("/diff   "), Some(Command::Diff(None)));
        assert_eq!(
            parse_command("/diff --base main"),
            Some(Command::Diff(Some("main".into())))
        );
        assert_eq!(
            parse_command("/diff --base foo/bar"),
            Some(Command::Diff(Some("foo/bar".into())))
        );
        assert_eq!(
            parse_command("/diff   --base   origin/main  "),
            Some(Command::Diff(Some("origin/main".into()))),
            "extra whitespace is not a word"
        );
        assert_eq!(parse_command("/diff --base"), Some(Command::DiffUsage));
        assert_eq!(parse_command("/diff --base --other"), Some(Command::DiffUsage));
        assert_eq!(parse_command("/diff foo"), Some(Command::DiffUsage));
        assert_eq!(parse_command("/diff --other"), Some(Command::DiffUsage));
        assert_eq!(parse_command("/diff --base main extra"), Some(Command::DiffUsage));
        // Only the exact token is `/diff`.
        assert_eq!(
            parse_command("/diffs x"),
            Some(Command::Forward("/diffs x".into()))
        );

        // ADR-0095: bare `/worktree` is the pending status; `--branch`/`--base`
        // adjust the intent before creation. The old card (`list`) and `new`
        // forms are retired and answer usage.
        assert_eq!(parse_command("/worktree"), Some(Command::WorktreeStatus));
        assert_eq!(
            parse_command("/worktree --branch fix-login"),
            Some(Command::WorktreeAdjust {
                branch: Some("fix-login".into()),
                base: None,
            })
        );
        assert_eq!(
            parse_command("/worktree --base main"),
            Some(Command::WorktreeAdjust {
                branch: None,
                base: Some("main".into()),
            })
        );
        assert_eq!(
            parse_command("/worktree --branch fix --base origin/main"),
            Some(Command::WorktreeAdjust {
                branch: Some("fix".into()),
                base: Some("origin/main".into()),
            })
        );
        for bad in [
            "/worktree list",
            "/worktree new fix",
            "/worktree --branch",
            "/worktree --branch a --branch b",
            "/worktree --base --other",
            "/worktree bogus",
        ] {
            assert_eq!(parse_command(bad), Some(Command::WorktreeUsage), "{bad}");
        }
        assert_eq!(
            parse_command("/worktrees"),
            Some(Command::Forward("/worktrees".into()))
        );
    }

    /// ADR-0095: the two worktree-creating forms take `--name` / `--branch` /
    /// `--base` in any order, each value one word. A positional word, a bare
    /// flag, a duplicate or an unknown flag is the usage answer.
    #[test]
    fn parse_worktree_flag_forms() {
        assert_eq!(
            parse_command("/topic --worktree"),
            Some(Command::TopicWorktree {
                name: None,
                branch: None,
                base: None,
            })
        );
        assert_eq!(
            parse_command("/topic --worktree --name 修复登录 --branch fix-login --base main"),
            Some(Command::TopicWorktree {
                name: Some("修复登录".into()),
                branch: Some("fix-login".into()),
                base: Some("main".into()),
            })
        );
        assert_eq!(
            parse_command("/new --worktree --branch fix-login --name Fix"),
            Some(Command::NewWorktree {
                name: Some("Fix".into()),
                branch: Some("fix-login".into()),
                base: None,
            })
        );
        assert_eq!(
            parse_command("/new --worktree --name Fix Login --base main"),
            Some(Command::NewWorktree {
                name: Some("Fix Login".into()),
                branch: None,
                base: Some("main".into()),
            }),
            "--name keeps the title's spaces"
        );
        for bad in [
            "/topic --worktree fix-login",
            "/topic --worktree --name",
            "/topic --worktree --name --branch x",
            "/topic --worktree --branch a --branch b",
            "/new --worktree --bogus x",
            "/new --base main",
            "/topic --base main",
            "/topic --adopt --worktree",
            "/topic --adopt --worktree foo",
        ] {
            assert_eq!(parse_command(bad), Some(Command::WorktreeUsage), "{bad}");
        }
        // The non-worktree forms are unchanged.
        assert_eq!(
            parse_command("/topic /r/p"),
            Some(Command::Topic {
                directory: Some("/r/p".into()),
                name: None,
            })
        );
        assert_eq!(parse_command("/new fix"), Some(Command::New(Some("fix".into()))));
    }

    /// `/run` grammar (ADR-0086 §6): flags only before the command, `--` ends
    /// flag parsing, and every refused form parses to `Run(None)` — the usage
    /// answer, never a Forward prompt and never a spawn.
    #[test]
    fn parse_run() {
        let run = |command: &str, timeout: std::time::Duration| {
            Some(Command::Run(Some(RunSpec {
                command: command.into(),
                timeout,
            })))
        };
        assert_eq!(parse_command("/run echo hi"), run("echo hi", RUN_DEFAULT_TIMEOUT));
        assert_eq!(parse_command("/RUN echo hi"), run("echo hi", RUN_DEFAULT_TIMEOUT));
        assert_eq!(
            parse_command("/run   echo   hi  "),
            run("echo   hi", RUN_DEFAULT_TIMEOUT),
            "the command keeps its inner spacing, trailing space is trimmed"
        );
        assert_eq!(parse_command("/run"), Some(Command::Run(None)));
        assert_eq!(parse_command("/run   "), Some(Command::Run(None)));

        assert_eq!(
            parse_command("/run --timeout 30s echo hi"),
            run("echo hi", std::time::Duration::from_secs(30))
        );
        assert_eq!(
            parse_command("/run --timeout 10m echo hi"),
            run("echo hi", std::time::Duration::from_secs(600))
        );
        assert_eq!(
            parse_command("/run --timeout 2h echo hi"),
            run("echo hi", std::time::Duration::from_secs(7_200))
        );
        assert_eq!(
            parse_command("/run --timeout 90 echo hi"),
            run("echo hi", std::time::Duration::from_secs(90)),
            "no unit means seconds"
        );
        assert_eq!(
            parse_command("/run --timeout 86400s echo hi"),
            run("echo hi", RUN_MAX_TIMEOUT),
            "exactly 24h is allowed"
        );
        // Refused forms: bare, malformed, past the cap, unknown flag.
        assert_eq!(parse_command("/run --timeout"), Some(Command::Run(None)));
        assert_eq!(parse_command("/run --timeout   "), Some(Command::Run(None)));
        assert_eq!(
            parse_command("/run --timeout abc echo hi"),
            Some(Command::Run(None))
        );
        assert_eq!(
            parse_command("/run --timeout 5x echo hi"),
            Some(Command::Run(None))
        );
        assert_eq!(
            parse_command("/run --timeout 5s5 echo hi"),
            Some(Command::Run(None))
        );
        assert_eq!(
            parse_command("/run --timeout -5s echo hi"),
            Some(Command::Run(None))
        );
        assert_eq!(
            parse_command("/run --timeout 25h echo hi"),
            Some(Command::Run(None))
        );
        assert_eq!(
            parse_command("/run --timeout 86401s echo hi"),
            Some(Command::Run(None))
        );
        assert_eq!(
            parse_command("/run --base main echo hi"),
            Some(Command::Run(None))
        );
        assert_eq!(
            parse_command("/run -ls"),
            Some(Command::Run(None)),
            "a dash-led token before `--` is a flag"
        );

        // `--` terminates flags: the rest is the command verbatim.
        assert_eq!(parse_command("/run -- -ls"), run("-ls", RUN_DEFAULT_TIMEOUT));
        assert_eq!(
            parse_command("/run -- --version"),
            run("--version", RUN_DEFAULT_TIMEOUT)
        );
        assert_eq!(
            parse_command("/run --timeout 5s -- --help me"),
            run("--help me", std::time::Duration::from_secs(5))
        );
        // Flags after the command are command text, never flags.
        assert_eq!(
            parse_command("/run echo --timeout 5s"),
            run("echo --timeout 5s", RUN_DEFAULT_TIMEOUT)
        );
        // Only the exact token is `/run`.
        assert_eq!(
            parse_command("/runner x"),
            Some(Command::Forward("/runner x".into()))
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

    /// `/run` is a documented command, not a `Forward` prompt (ADR-0086 §1):
    /// the index lists it, and `/help run` carries both entry points, the cap
    /// and `--timeout`, the absent terminal and the Windows refusal.
    #[test]
    fn help_lists_run_and_its_topic_documents_the_terminal_contract() {
        let text = help_text();
        assert!(text.contains("`/run"), "the command index lists `/run`: {text}");
        let run = command_help("run").expect("run has a help topic");
        assert!(run.contains("/run"), "{run}");
        assert!(run.contains("!<cmd>"), "the message prefix is documented: {run}");
        assert!(run.contains("--timeout"), "the flag is documented: {run}");
        assert!(
            run.contains("10 minutes") && run.contains("24 hours"),
            "the default cap and hard maximum are documented: {run}"
        );
        assert!(
            run.contains("no pseudo-terminal") || run.contains("no terminal"),
            "the absent terminal is documented: {run}"
        );
        assert!(
            run.contains("Windows"),
            "the Windows refusal is documented: {run}"
        );
    }

    /// `/card`'s help describes ALL THREE states (ADR-0100): the pull, the
    /// busy/waiting follow, and the one-shot read it falls back to — never the
    /// retired 「当前没有正在运行的实时卡片」 notice. The index line names them
    /// too.
    #[test]
    fn help_documents_the_card_reads_three_states() {
        let index = help_text();
        assert!(
            index.contains("read the Session when no Turn is rendering"),
            "the index names the read: {index}"
        );
        assert!(
            index.contains("follow a run cola did not start"),
            "the index names the follow: {index}"
        );
        let card = command_help("card").expect("card has a help topic");
        assert!(card.contains("部分完成"), "the pull is documented: {card}");
        assert!(
            card.contains("follows that run to its end"),
            "the busy/waiting follow is documented: {card}"
        );
        assert!(card.contains("read-only 会话 card"), "{card}");
        assert!(card.contains("no button, no form"), "{card}");
        assert!(
            card.contains("已由新卡片接管"),
            "the supersede is documented: {card}"
        );
        assert!(
            card.contains("见原卡片"),
            "the pending pointer's copy is documented: {card}"
        );
    }

    #[test]
    fn command_help_known_and_unknown() {
        assert!(command_help("model").unwrap().contains("/model"));
        assert!(command_help("think").unwrap().contains("/think"));
        assert!(command_help("dir").unwrap().contains("next non-command message"));
        let cat = command_help("cat").expect("cat has a help topic");
        assert!(cat.contains("/cat <path>"), "{cat}");
        assert!(cat.contains("/cat <directory>"), "{cat}");
        assert!(
            cat.contains("plain_text"),
            "the honest degradation is documented: {cat}"
        );
        assert!(cat.contains("相对") || cat.contains("Path Scope"), "{cat}");
        assert!(command_help("version").unwrap().contains("/version"));
        assert!(command_help("status").unwrap().contains("/status"));
        assert!(command_help("sub").unwrap().contains("/sub list"));
        assert!(
            command_help("sub").unwrap().contains("/sub attach"),
            "the sub help documents the takeover form"
        );
        let skill = command_help("skill").expect("skill has a help topic");
        assert!(
            skill.contains("#<id>"),
            "the skill help shows the # syntax: {skill}"
        );
        assert!(
            skill.contains("#implement-spec #foreman"),
            "the skill help documents the repeated-token form: {skill}"
        );
        assert!(
            help_text().contains("/diff [--base <ref>]"),
            "/diff is listed in the command index: {}",
            help_text()
        );
        let diff = command_help("diff").expect("diff has a help topic");
        assert!(diff.contains("/diff [--base <ref>]"), "{diff}");
        assert!(diff.contains("稍后再试"), "the cooldown is documented: {diff}");
        let worktree = command_help("worktree").expect("worktree has a help topic");
        assert!(worktree.contains("--branch"), "{worktree}");
        assert!(worktree.contains("--base"), "{worktree}");
        assert!(worktree.contains("git worktree list"), "{worktree}");
        assert!(
            help_text().contains("/topic --worktree"),
            "/worktree is listed in the command index: {}",
            help_text()
        );
        assert!(
            command_help("new").unwrap().contains("--worktree"),
            "/new documents the worktree flag"
        );
        assert!(
            command_help("topic").unwrap().contains("--worktree"),
            "/topic documents the worktree flag"
        );
        assert_eq!(command_help("nonexistent"), None);
        // The retired text-list form has no help topic of its own.
        assert_eq!(command_help("list"), None);
    }

    /// The skill signatures are discoverable in every help surface (spec #652,
    /// ticket #657; the `#<id>` form added in spec #662): the plain-text
    /// fallback the `/help` card degrades to must list them with their syntax,
    /// exactly like the card and the per-command topic do.
    #[test]
    fn help_text_lists_the_skill_command() {
        let text = help_text();
        assert!(text.contains("`#<id>"), "help text lists the # form: {text}");
        assert!(text.contains("`/skill`"), "help text lists /skill: {text}");
    }

    #[test]
    fn unknown_command_forwarded() {
        let cmd = parse_command("/init");
        assert_eq!(cmd, Some(Command::Forward("/init".into())));

        let cmd2 = parse_command("/some-unknown-command arg1");
        assert_eq!(cmd2, Some(Command::Forward("/some-unknown-command arg1".into())));
    }

    /// `/skill <id>` is its own command, not forwarded text (spec #652, ticket
    /// #654): each `/skill` token contributes the token after it, in order, and
    /// a trailing argument that follows no `/skill` token stays prompt text. Its
    /// dispatch owns the picker (`picker_on_empty: true`).
    #[test]
    fn skill_command_collects_ids_in_order() {
        assert_eq!(
            parse_command("/skill implement-spec 644"),
            Some(Command::Skill(SkillInvocation {
                ids: vec!["implement-spec".into()],
                picker_on_empty: true,
            }))
        );
        assert_eq!(
            parse_command("/skill implement-spec /skill foreman 644"),
            Some(Command::Skill(SkillInvocation {
                ids: vec!["implement-spec".into(), "foreman".into()],
                picker_on_empty: true,
            }))
        );
        // Case-insensitive command token, like every other command.
        assert_eq!(
            parse_command("/SKILL implement-spec"),
            Some(Command::Skill(SkillInvocation {
                ids: vec!["implement-spec".into()],
                picker_on_empty: true,
            }))
        );
        // A bare `/skill` names no skill.
        assert_eq!(
            parse_command("/skill"),
            Some(Command::Skill(SkillInvocation {
                ids: vec![],
                picker_on_empty: true,
            }))
        );
        // `/skill` is the command only when it LEADS the message; anywhere else
        // the message is ordinary text (or a different command).
        assert_eq!(
            parse_command("/init /skill foo"),
            Some(Command::Forward("/init /skill foo".into()))
        );
        // Inside a `/`-leading message the slash grammar owns the parse — a `#`
        // token there is not collected.
        assert_eq!(
            parse_command("/skill a #b"),
            Some(Command::Skill(SkillInvocation {
                ids: vec!["a".into()],
                picker_on_empty: true,
            }))
        );
    }

    /// A `#` opens a token only at a left boundary (spec #662): one wedged
    /// inside a word (`C#caveman`, `abc#foreman`) or directly after another `#`
    /// (`##foreman`) is prose, so `C#`-style text and heading-shaped runs never
    /// false-trigger a skill load.
    #[test]
    fn hash_tokens_need_a_left_boundary() {
        // A `#` wedged inside a word is prose, not a skill token: a letter or
        // digit or `-` before it means the `#` sits inside a word (`C#caveman`,
        // `abc#foreman`, `a-#foreman`), and a `#` before it means a
        // heading-shaped run (`##foreman`). None of them open a token.
        assert_eq!(parse_command("用的是 C#caveman 吗"), None);
        assert_eq!(parse_command("abc#foreman"), None);
        assert_eq!(parse_command("a-#foreman"), None);
        assert_eq!(parse_command("##foreman"), None);
        // A real boundary — start of text, whitespace, CJK, punctuation — opens
        // the token as before.
        assert_eq!(
            parse_command("没有#caveman试试"),
            Some(Command::Skill(SkillInvocation {
                ids: vec!["caveman".into()],
                picker_on_empty: false,
            }))
        );
        assert_eq!(
            parse_command("(#caveman)"),
            Some(Command::Skill(SkillInvocation {
                ids: vec!["caveman".into()],
                picker_on_empty: false,
            }))
        );
    }

    /// A message that does not lead with `/` still loads skills with `#<id>`
    /// tokens (spec #662): the ids are collected in order, `picker_on_empty` is
    /// false (a `#` dispatch owns no picker), and the whole message stays the
    /// prompt. Only a `#` immediately followed by an ASCII letter counts, so
    /// prose with `#644` or a markdown heading is an ordinary prompt — no skill
    /// work.
    #[test]
    fn hash_skill_invocation_collects_letter_ids() {
        assert_eq!(
            parse_command("#implement-spec 644"),
            Some(Command::Skill(SkillInvocation {
                ids: vec!["implement-spec".into()],
                picker_on_empty: false,
            }))
        );
        assert_eq!(
            parse_command("#implement-spec #foreman 644"),
            Some(Command::Skill(SkillInvocation {
                ids: vec!["implement-spec".into(), "foreman".into()],
                picker_on_empty: false,
            }))
        );
        // A mid-message token counts too; the whole text stays the prompt.
        assert_eq!(
            parse_command("用 #caveman 再 #implement-spec 644"),
            Some(Command::Skill(SkillInvocation {
                ids: vec!["caveman".into(), "implement-spec".into()],
                picker_on_empty: false,
            }))
        );
        // No `#<letter…>` token: an ordinary prompt, no skill-list read.
        assert_eq!(parse_command("fix #644 please"), None);
        assert_eq!(parse_command("# 标题\n正文"), None);
        assert_eq!(parse_command("bare # trailing"), None);
        assert_eq!(parse_command("## not a skill"), None);
        assert_eq!(parse_command("#1abc not a skill"), None);
        // The id run ends at a non-id character: trailing punctuation and CJK
        // text run straight into the token still name the skill.
        assert_eq!(
            parse_command("see #implement-spec, then"),
            Some(Command::Skill(SkillInvocation {
                ids: vec!["implement-spec".into()],
                picker_on_empty: false,
            }))
        );
        assert_eq!(
            parse_command("用#caveman试试"),
            Some(Command::Skill(SkillInvocation {
                ids: vec!["caveman".into()],
                picker_on_empty: false,
            }))
        );
    }

    #[test]
    fn trailing_spaces_ignored() {
        assert_eq!(
            parse_command("  /switch list  "),
            Some(Command::Switch(SwitchAction::Match("list".into())))
        );
    }

    #[test]
    fn case_insensitive_command() {
        assert_eq!(parse_command("/STOP"), Some(Command::Stop));
        assert_eq!(
            parse_command("/Switch List"),
            Some(Command::Switch(SwitchAction::Match("List".into())))
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
        // Bare `~` resolves to home itself. normalize_directory canonicalizes,
        // which can normalize the home spelling (\\?\ prefix on Windows), so
        // compare against the canonicalized home.
        let home_only = normalize_directory("~");
        assert_eq!(
            home_only,
            std::fs::canonicalize(&home).unwrap_or_else(|_| home.clone()),
            "bare ~ must resolve to home"
        );
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
        // normalize_directory canonicalizes the cwd; on Windows that adds a
        // \\?\ verbatim prefix, so compare against the canonicalized form.
        assert_eq!(
            abs,
            std::fs::canonicalize(&cwd).unwrap_or_else(|_| cwd.clone()),
            "an existing absolute path canonicalizes to itself"
        );
        // A nonexistent absolute path is preserved (caller reports it missing).
        // The parent (a tempdir) exists but the leaf does not, so canonicalize
        // fails and normalize_directory keeps the expanded absolute form. This
        // is platform-neutral (no Unix-root assumption like `/nonexistent`).
        let base = tempfile::tempdir().unwrap();
        let target = base.path().join("nonexistent-subdir");
        let missing = normalize_directory(&target.to_string_lossy());
        assert_eq!(
            missing, target,
            "a nonexistent absolute path is preserved for the caller to report"
        );
    }

    /// `lexical_normalize` collapses `.`/`..` textually, without touching the
    /// filesystem — the browser's path resolution applies it to the tail of a
    /// not-yet-existing target (#670 review).
    #[test]
    fn lexical_normalize_collapses_dot_dot_textually() {
        use std::path::Path;
        assert_eq!(lexical_normalize(Path::new("/a/b/../../c")), Path::new("/c"));
        assert_eq!(lexical_normalize(Path::new("/a/./b/")), Path::new("/a/b"));
        assert_eq!(lexical_normalize(Path::new("/..")), Path::new("/"));
        assert_eq!(lexical_normalize(Path::new("a/../b")), Path::new("b"));
        assert_eq!(lexical_normalize(Path::new("../a")), Path::new("../a"));
    }
}
