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
    /// Create a real Feishu topic backed by a fresh session. `directory` is
    /// `None` for the bare `/topic` form, which inherits the conversation's
    /// current project (the active session's directory, ADR-0012) instead of an
    /// explicit path.
    Topic {
        directory: Option<String>,
        name: Option<String>,
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
    /// Show available commands, or help for one command (`/help <cmd>`).
    Help(Option<String>),
    /// Forward unrecognized slash command to OpenCode as prompt text
    Forward(String),
}

impl Command {
    /// The single source of truth for the "commands restricted inside a topic"
    /// rule (ADR-0007, ADR-0023): `handle_command` calls this at its top.
    ///
    /// - `/topic` and its adopt forms would nest a topic inside a topic: they
    ///   are rejected in ANY topic, whether or not it already has a session.
    /// - Session selection/creation (`/dir`, the `/dir` card, `/switch`,
    ///   `/sub attach`, `/new`) is allowed while the topic is unbound — the
    ///   chosen session becomes the topic's one session — and rejected once the
    ///   topic owns a session.
    /// - Every other command passes.
    pub(crate) fn topic_rejection(&self, has_session: bool) -> Option<&'static str> {
        match self {
            Command::Topic { .. } => Some(TOPIC_NEST_REJECTION),
            Command::TopicAdopt { .. } | Command::TopicAdoptCard => Some(TOPIC_ADOPT_NEST_REJECTION),
            Command::Dir(_)
            | Command::DirCard
            | Command::Switch(_)
            | Command::Sub(SubAction::Attach { .. })
            | Command::New(_)
                if has_session =>
            {
                Some(TOPIC_SELECTION_REJECTION)
            }
            _ => None,
        }
    }
}

/// The rejection for `Dir`/`DirCard`/`Switch`/`New` inside a topic that
/// already owns a session (ADR-0007): a bound topic is a conversation of its
/// own, so sessions are chosen from the main conversation.
pub(crate) const TOPIC_SELECTION_REJECTION: &str = "⚠️ 话题已绑定会话，请回主对话操作。";

/// The rejection for `/topic` inside any topic (ADR-0006, ADR-0023): a topic
/// never nests inside another topic, bound or not.
pub(crate) const TOPIC_NEST_REJECTION: &str =
    "⚠️ /topic 只能从会话顶层使用，不能在话题里再开话题。请在主会话里发 /topic <目录>。";

/// The rejection for `/topic --adopt` inside any topic — one text shared by
/// the keyword form and the no-arg picker card (ADR-0016, ADR-0023).
pub(crate) const TOPIC_ADOPT_NEST_REJECTION: &str =
    "⚠️ /topic --adopt 只能从会话顶层使用，不能在话题里开话题。请在主会话里发 /topic --adopt <会话>。";

/// `/card` with no live Card to pull down (ADR-0043, 2026-09-22 amendment):
/// no Turn is rendering, so there is nothing to move — one text notice, no
/// card.
const NO_LIVE_CARD: &str = "当前没有正在运行的实时卡片。";

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
        "/new" => Some(Command::New(arg.map(|s| s.to_string()))),
        "/topic" => match arg {
            Some(a) => {
                // `/topic --adopt <keyword> [--force]` — adopt an existing
                // session into a new topic; `/topic --adopt` pops the card.
                if a.split_whitespace().next() == Some("--adopt") {
                    let mut words: Vec<&str> = Vec::new();
                    let mut force = false;
                    for w in a.split_whitespace().skip(1) {
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
        "/help" => Some(Command::Help(arg.map(|s| s.to_lowercase()))),
        // `/init`, `/review`, or any unknown /command — forward to OpenCode
        _ => Some(Command::Forward(trimmed.to_string())),
    }
}

/// Help text shown for the `/help` command.
pub fn help_text() -> String {
    "\
**cola commands**
`/dir <path> [name]` · Declare a new session in a project directory (created by the next message)
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
`/card` · Pull the live card back to the newest position (after command replies bury it)
`/agent <name>` · Switch agent (takes effect next message)
`/model <p/m>` · Switch model (takes effect next message)
`/think [等级]` · Set/clear thinking level (per model; takes effect next message)
`/autoaccept` · Show auto-approve status; `/autoaccept on|off` switches
`/restart` · Restart cola (keeps startup args + log redirect)
`/restart-opencode` · Restart the OpenCode server (only when cola started it)
`/update` · Update cola (GitHub Releases self-update; cargo installs get the cargo command)
`/version` · Show cola version & build provenance (release / crates.io / dev build)
`/help <command>` · Show help for one command (e.g. `/help model`)

话题规则：已绑定会话的话题里，`/switch`、`/new`、`/dir`、`/sub attach` 被拒绝，请回主对话操作。从未绑定过会话的话题可以用它们来绑定该话题的唯一会话。
    "
    .to_string()
}

/// Detailed help for one command (`/help <command>`). `None` for unknown names.
pub fn command_help(name: &str) -> Option<String> {
    let text = match name.to_lowercase().as_str() {
        "dir" => {
            "/dir <path> [name]\nDeclare a new session rooted at <path>: nothing is created yet — the conversation's next non-command message creates the session in that directory and maps it here, so a mistaken `/dir` can be corrected with another `/dir`, `/new`, `/switch` or `/topic` and leaves no session behind.\n- `/dir` (no arg) — Recent Directories card: pick a recently-used folder and declare it there, or open it as a fresh topic (each row's 建话题 = `/topic <dir>` without typing; only from the main conversation). When there are many directories, the card carries a search box over the paths.\nExample: `/dir /root/proj/lib`"
        }
        "switch" => {
            "/switch [action]\nSession management card and text forms.\n- `/switch` (no arg) — interactive session card (browse / search / adopt / new)\n- `/switch <keyword>` — switch by title/directory/id; the current chat's sessions win, otherwise a unique global match is adopted. Ambiguous keywords list candidates; no match opens the card pre-filtered by the keyword.\n- `/switch <id|title> [--force]` — take over a session (exact id → id-prefix → title; the card's short hash works too; reject if owned by another chat unless `--force`, or use the card's 强制接管 button)\n- `/switch forget` — un-map this chat's session (server session stays)\nExamples: `/switch backend`, `/switch ses_abc --force`"
        }
        "sub" => {
            "/sub [list [keyword]] | /sub attach <id|id-prefix|title> [--force]\nShow the child sessions of the current Active Session, read-only: each row carries the child's title, id tail, agent, last activity and live run state (运行中 when Busy/Retry, 空闲 when Idle — one status read per listed row). The view is scoped to THIS session: children of other sessions never appear, and only direct children are listed (no nested descendants, no transcripts).\n- `/sub` / `/sub list` — the child-session card\n- `/sub list <keyword>` — the same card pre-filtered (title/directory/id, whitespace-token AND)\n- `/sub attach <id|id-prefix|title> [--force]` — take over one of the current session's DIRECT children into this chat (the child becomes the Active Session and gets the usual Session Snapshot receipt). The query resolves like `/switch` (exact id → unique id-prefix → title substring) but only among this session's direct children: an unknown or ambiguous query is reported, and a session that is not a direct child is refused. If the child is mapped to another chat, the owner is named and it is refused unless `--force` (which steals the mapping). Taking over a running child is allowed and the snapshot shows its live state; re-running it for the already-active child changes nothing. The parent stays mapped — `/switch` switches back to it.\nThe card's search box and pagination keep the active keyword/page through every rebuild (six rows per page, ADR-0052). Without an Active Session (a fresh chat, or a Pending Session declared by `/new`/`/dir`/`/topic`) the card opens with a plain empty state.\nExamples: `/sub`, `/sub list 渲染`, `/sub attach 1a2b3c4`, `/sub attach 渲染 --force`"
        }
        "attach" => {
            "/switch <id|title> [--force]\nTake over a session created outside Feishu into this chat. Resolution: exact id → unique id-prefix (the short hash shown on the card works too) → unique title substring. If the session already belongs to another chat, show its owner and reject unless `--force`.\nExample: `/switch ses_abc123`"
        }
        "forget" => {
            "/switch forget\nUn-map this chat's session. The server session stays untouched and can be adopted again.\nExample: `/switch forget`"
        }
        "new" => {
            "/new [name]\nDeclare a new session in the current project (the active session's directory, or the pending's when one is already declared); with neither, the default directory (`work_dir` or cwd). Nothing is created yet: the conversation's next non-command message creates the session and maps it here, so a mistaken `/new` can be corrected with another `/new`, `/dir`, `/switch` or `/topic` and leaves no session behind. The optional name becomes the created session's server title.\nExample: `/new api-refactor`"
        }
        "topic" => {
            "/topic [dir] [name]\nCreate a real Feishu topic for a new session. The topic is UI-separated from the current conversation, so you can switch between topics in the Feishu client. Opening the topic creates NO session yet: the topic's first non-command message creates the session in the chosen directory and maps it here, so a mistaken `/topic` leaves nothing in the shared store — correct it in place with `/switch <id>` (or `/dir`/`/new`) inside the topic.\n- `/topic` (no args) — new session in the CURRENT PROJECT (the active session's directory, like `/new`; falls back to the default directory when the conversation has no session)\n- `/topic <dir>` — new session rooted at <dir>\n- `/topic <dir> <name>` — also name the session\nExample: `/topic /root/proj/lib api-refactor`\n\n/topic --adopt <keyword> [--force]\nOpen a topic around an EXISTING session instead of creating a new one. Resolution: exact id → unique id-prefix (the short hash shown on the card works too) → unique title substring (the whole remaining arg is the keyword, so multi-word titles match). Child (sub-task) sessions are rejected. If the session belongs to another chat, reject unless `--force` (which steals the mapping). No argument pops the session card — each row's 建话题接管 button does the same, and an occupied session offers a 强制建话题接管 confirmation.\nExample: `/topic --adopt 重写登录模块`"
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
            "/card\nPull the live card back to the newest position: split the Card Chain at this command, so a continuation card becomes the newest message and keeps receiving updates (ADR-0043). Use it after running commands mid-turn — their replies deliberately stay the newest messages, which buries the live card above them. Nothing happens without a running Turn: cola replies a notice. The previous card is finalized with the 部分完成，继续中 header and keeps everything streamed before the pull; the continuation carries the status line and only the content that arrives after it."
        }
        "agent" => {
            "/agent <name>\nSwitch the agent for the current session — a per-session override sent on the NEXT message (the OpenCode server has no agent-switch endpoint). Without an override the server's default agent applies; the card (`/agent` alone) shows the current one. `--reset` clears the override back to the server default. Persisted across restarts. On a Pending Session (`/new`/`/dir`/`/topic` before its first message) the override is recorded on the pending and applies to the session the first message creates. Unknown agent names surface as an error on the next prompt.\nExample: `/agent build`"
        }
        "model" => {
            "/model <provider/model>\nSwitch the model for the current session — a per-session override sent on the NEXT message (the server has no model-switch endpoint; unset = the configured default / server default). Persisted across restarts. On a Pending Session (`/new`/`/dir`/`/topic` before its first message) the override is recorded on the pending and applies to the session the first message creates.\nExample: `/model opencode-go/deepseek-v4-flash`"
        }
        "think" => {
            "/think [等级]\nSet or clear the thinking level for the current session — a per-session override sent as `variant` on the NEXT message. Each model declares its own levels (e.g. `low`/`high`/`minimal`), so there is no universal scale: the card (`/think` alone) lists what the current model supports, and switching to a model that doesn't declare the current level clears it. `--reset` clears the override (= the server's default for the model). Persisted across restarts. On a Pending Session (`/new`/`/dir`/`/topic` before its first message) the level is recorded on the pending and applies to the session the first message creates.\nExample: `/think high`"
        }
        "autoaccept" => {
            "/autoaccept [on|off]\nShow or switch auto-allowing permission requests for this session (no permission cards). On a Pending Session (`/new`/`/dir`/`/topic` before its first message) the flag is recorded on the pending and applies to the session the first message creates.\nNo arg: show current state. `/autoaccept on` / `/autoaccept off` switch it.\nExample: `/autoaccept`"
        }
        "restart" => {
            "/restart\nRestart cola itself, keeping startup args and the log redirect. The new process takes over the singleton lock (passes --replace). Under a systemd unit cola exits and lets `Restart=on-failure` bring it back; elsewhere it re-execs. cola announces in this chat when it's back."
        }
        "restart-opencode" => {
            "/restart-opencode\nRestart the OpenCode server. cola only restarts a server IT started; a server launched by another tool is left alone and needs a manual restart."
        }
        "update" => {
            "/update\nUpdate cola to the latest version (ADR-0030).\n- GitHub Releases install: check GitHub Releases; if newer, download, verify (SHA256SUMS), replace the running binary and restart. When running as a systemd unit, the restart hands back to `Restart=on-failure`; otherwise the new process re-execs with --replace.\n- cargo install (`cargo install colark` / `cargo binstall colark`, detected from cargo's install receipt): nothing is replaced; cola reports the crates.io version and the command to run (`cargo install colark`; binstall users `cargo binstall colark`; add `--force` if cargo says already installed).\nCLI equivalent: `cola update [--check]`."
        }
        "version" => {
            "/version\nShow the running cola's version and build provenance (no network).\n- Release build (built at a clean release tag): `cola 0.7.0`\n- crates.io install (`cargo install colark`): `cola 0.8.1 (crates.io)`\n- Dev build (local or untagged): `cola 0.7.0-dev <branch>@<sha> ⚠` — ⚠ means the source tree had uncommitted changes at build time\nUpdate checks compare only the release version, so a dev build is never reported outdated by its dev marker.\nCLI equivalent: `cola --version`."
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
use crate::bridge::handles::CommandHandles;
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
fn write_restart_notify(notify: &RestartNotify) {
    if let Ok(raw) = serde_json::to_string(notify) {
        let _ = std::fs::write(restart_notify_path(), raw);
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

/// Execute a parsed slash command against the shared handles. Unrecognized
/// `/command`s are intercepted by the message coordinator (which owns the
/// prompt pipeline), so this never forwards — the `Command::Forward` arm below
/// is unreachable and kept only for exhaustiveness.
pub(crate) async fn handle_command(
    handles: &CommandHandles,
    cmd: Command,
    thread_key: ThreadKey,
    message_id: &str,
    kind: ConversationKind,
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
                    let settings = handles.flow.sessions.session_settings(&thread_key).await;
                    let mut approved = Vec::new();
                    if on
                        && let Some(s) = settings.as_ref()
                        && let Some(id) = s.session_id.as_deref()
                    {
                        approved = crate::bridge::request::kind::approve_pending_for_session(
                            &handles.flow.sessions,
                            &handles.flow.requests,
                            &handles.flow.backend,
                            id,
                            &s.directory,
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
                    if let Some(mut s) = settings {
                        s.auto_accept = on;
                        handles.flow.sessions.set_session_settings(&thread_key, s).await?;
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
                handles.flow.backend.interrupt(&id).await?;
                // Mark the session stopped so a running post-prompt drain
                // (ADR-0043) finalizes promptly instead of waiting out its
                // bound on a Supplement the abort left unanswered. The next
                // Turn clears the marker when it starts.
                handles.flow.waits.stopped_sessions.lock().await.insert(id);
                handles
                    .flow
                    .platform
                    .reply_text(message_id, "Interrupted.")
                    .await?;
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
                handles
                    .flow
                    .platform
                    .reply_text(message_id, "Compacting...")
                    .await?;
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
                handles.flow.platform.reply_text(message_id, NO_LIVE_CARD).await?;
                return Ok(());
            };
            let running = crate::bridge::turn::Turn::is_running(&handles.flow.cards, &session_id).await;
            if !running {
                handles.flow.platform.reply_text(message_id, NO_LIVE_CARD).await?;
                return Ok(());
            }
            tracing::info!("card pull: session {session_id} live card split requested");
            crate::bridge::turn::Turn::split_card_chain(
                &handles.flow.cards,
                &session_id,
                message_id,
                crate::bridge::turn::SplitKind::Pull,
            )
            .await;
        }
        Command::AgentCard => {
            crate::feishu::card::command::send_agent_card(handles, &thread_key, message_id).await?;
        }
        Command::Agent(name) => {
            // The OpenCode server has no agent-switch endpoint (the legacy
            // `/api/session/{id}/agent` route 500s, same as `/model`'s dead
            // route), so `/agent` records a per-session override here — persisted
            // with the session mapping so it survives a restart — and cola sends
            // it as a per-prompt agent on the next message (the server honors
            // `PromptInput.agent`). On a Pending Session the override is
            // recorded on the pending and lands on the created session at
            // materialisation (ADR-0041). Unknown agent names surface as a
            // clear error on the next prompt's card. `--reset` clears the
            // override (the server's default agent applies); an agent literally
            // named `default`/`off`/`reset` is a normal pick, never a clear word.
            let Some(mut settings) = handles.flow.sessions.session_settings(&thread_key).await else {
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
            settings.agent = if cleared { None } else { Some(name.clone()) };
            handles
                .flow
                .sessions
                .set_session_settings(&thread_key, settings)
                .await?;
            let msg = if cleared {
                "已清除 Agent（回到服务器默认）。".to_string()
            } else {
                format!("Agent: {}（下一条消息开始生效）", name)
            };
            handles.flow.platform.reply_text(message_id, &msg).await?;
        }
        Command::ModelCard => {
            crate::feishu::card::command::send_model_card(handles, &thread_key, message_id).await?;
        }
        Command::Model(name) => {
            // The OpenCode server has NO model-switch endpoint (the legacy
            // `/api/session/{id}/model` route is gone), so `/model` records
            // a per-session override here and cola sends it as a per-prompt
            // model on the next message. Validate the shape up front so a
            // typo gets immediate feedback instead of a silent no-op. On a
            // Pending Session the override is recorded on the pending and
            // lands on the created session at materialisation (ADR-0041).
            let Some(_) = crate::opencode::parsing::parse_model(&name) else {
                handles.flow.platform
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
            let Some(mut settings) = handles.flow.sessions.session_settings(&thread_key).await else {
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
            settings.model = Some(name.clone());
            // Auto-clear the `/think` variant when the new model doesn't
            // declare it (ADR-0020), shared with the `/model` picker card.
            let cleared_variant = handles
                .flow
                .sessions
                .clear_variant_for_model(&handles.flow.backend, &mut settings.variant, &name)
                .await;
            handles
                .flow
                .sessions
                .set_session_settings(&thread_key, settings)
                .await?;
            let extra = cleared_variant
                .map(|v| format!("（已清除思考等级 `{v}`：新模型不支持）"))
                .unwrap_or_default();
            handles
                .flow
                .platform
                .reply_text(
                    message_id,
                    &format!("Model: {}（下一条消息开始生效）{}", name, extra),
                )
                .await?;
        }
        Command::ThinkCard => {
            crate::feishu::card::command::send_think_card(handles, &thread_key, message_id).await?;
        }
        Command::Think(name) => {
            // The OpenCode server has no thinking-level endpoint either —
            // `/think` records a per-session variant override and cola sends it
            // as a per-prompt `variant` on the next message. On a Pending
            // Session the override is recorded on the pending and lands on the
            // created session at materialisation (ADR-0041). `--reset` clears
            // the override (the server's default for the model); a variant
            // literally named `default`/`off`/`reset` is a normal pick, never
            // a clear word.
            let Some(mut settings) = handles.flow.sessions.session_settings(&thread_key).await else {
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
                && let Some((provider, model)) = handles
                    .flow
                    .sessions
                    .effective_model(&handles.flow.backend, &settings)
                    .await
                && let Some(variants) = handles
                    .flow
                    .sessions
                    .model_variants(&handles.flow.backend, &provider, &model)
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
                            "⚠️ 当前模型 `{provider}/{model}` 不支持思考等级 `{name}`。可用：{available}。（清除请用 `/think --reset`）"
                        ),
                    )
                    .await?;
                return Ok(());
            }
            settings.variant = if cleared { None } else { Some(name.clone()) };
            handles
                .flow
                .sessions
                .set_session_settings(&thread_key, settings)
                .await?;
            let msg = if cleared {
                "已清除思考等级（回到模型默认）。".to_string()
            } else {
                format!("Thinking: {}（下一条消息开始生效）", name)
            };
            handles.flow.platform.reply_text(message_id, &msg).await?;
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
            write_restart_notify(&RestartNotify::new(&thread_key, message_id));
            // Under a systemd unit, do NOT spawn a child: `KillMode=control-group`
            // would kill it when the unit stops. Exit with the supervisor-restart
            // code and let `Restart=on-failure` bring the unit back up from the
            // same ExecStart (ADR-0015, ADR-0021 — same as `/update`).
            if let Some(code) = crate::update::supervisor_restart_code() {
                std::process::exit(code);
            }
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
            match crate::bridge::discovery::restart_self_spawned_server().await {
                Ok(crate::bridge::discovery::RestartOutcome::Restarted) => {
                    handles
                        .flow
                        .platform
                        .reply_text(message_id, "♻️ 已重启 OpenCode 服务器。")
                        .await?;
                }
                Ok(crate::bridge::discovery::RestartOutcome::NotOwned) => {
                    handles
                        .flow
                        .platform
                        .reply_text(
                            message_id,
                            "这个 OpenCode 服务器不是 cola 启动的，需要你手动重启它。",
                        )
                        .await?;
                }
                Ok(crate::bridge::discovery::RestartOutcome::NoServer) => {
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
                write_restart_notify(&notify);
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
            let removed = handles.flow.sessions.remove_thread_sessions(thread_key).await?;
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
///    a unique hit switches without changing the mapping. The re-switch ack
///    is a Session Snapshot card (已切换) unless the suppression predicate
///    says there is nothing to report (ADR-0028).
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
    let thread_hits: Vec<&crate::opencode::types::SessionListInfo> = sessions
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
            handles.flow.sessions.activate(entry).await?;
        }
        // ADR-0028 suppression: re-activating a session already mapped to
        // this thread reports a snapshot only when there is content to show —
        // busy/retry, a pending request, or an external newest message. An
        // idle, pending-free, cola-authored session keeps today's one-line
        // text ack (its recent life is already visible in this thread). The
        // `already_mapped` input is true by construction: the hit came from
        // this thread's mapped-session list. The gather and the 切换 card's
        // build run inside the Session's `snapshot` span (ADR-0048).
        match crate::bridge::snapshot::re_switch_snapshot(
            &handles.snapshot_handles(),
            thread_key,
            &hit.id,
            &hit.directory,
            &hit.title,
            // The text form has no card list to return to.
            None,
        )
        .await
        {
            crate::bridge::snapshot::ReSwitchSnapshot::Full { card, data } => {
                // Claim the snapshot's embedded pendings against the sent card
                // so the poll loop never duplicates them.
                let mid = handles.flow.platform.reply_card(message_id, &card).await?;
                crate::bridge::external::settle_snapshot_after_send(
                    &handles.external,
                    &handles.flow,
                    &mid,
                    "切换",
                    &hit.title,
                    &data,
                    None,
                )
                .await;
            }
            crate::bridge::snapshot::ReSwitchSnapshot::Suppressed => {
                handles
                    .flow
                    .platform
                    .reply_text(message_id, &format!("Switched to \"{}\".", hit.title))
                    .await?;
            }
        }
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
    let global_hits: Vec<&crate::opencode::types::SessionListInfo> = sessions
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
    Hit(&'a crate::opencode::types::SessionListInfo),
    /// Several sessions matched; the caller should list candidates.
    Ambiguous(Vec<&'a crate::opencode::types::SessionListInfo>),
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
    sessions: &'a [crate::opencode::types::SessionListInfo],
    query: &str,
) -> SessionResolution<'a> {
    let lower = query.to_lowercase();
    if let Some(s) = sessions.iter().find(|s| s.id == query) {
        return SessionResolution::Hit(s);
    }
    let full_prefix: Vec<&crate::opencode::types::SessionListInfo> = sessions
        .iter()
        .filter(|s| s.id.to_lowercase().starts_with(&lower))
        .collect();
    if let Some(r) = decide_id_hits(full_prefix) {
        return r;
    }
    let bare_prefix: Vec<&crate::opencode::types::SessionListInfo> = sessions
        .iter()
        .filter(|s| {
            let id = s.id.to_lowercase();
            id.strip_prefix("ses_").unwrap_or(&id).starts_with(&lower)
        })
        .collect();
    if let Some(r) = decide_id_hits(bare_prefix) {
        return r;
    }
    let suffix: Vec<&crate::opencode::types::SessionListInfo> = sessions
        .iter()
        .filter(|s| {
            let id = s.id.to_lowercase();
            id.strip_prefix("ses_").unwrap_or(&id).ends_with(&lower)
        })
        .collect();
    if let Some(r) = decide_id_hits(suffix) {
        return r;
    }
    let titles: Vec<&crate::opencode::types::SessionListInfo> = sessions
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
    hits: Vec<&'a crate::opencode::types::SessionListInfo>,
) -> Option<SessionResolution<'a>> {
    match hits.len() {
        0 => None,
        1 => Some(SessionResolution::Hit(hits[0])),
        _ => Some(SessionResolution::Ambiguous(hits)),
    }
}

/// `/attach <id|title> [--force]` — take over an arbitrary server session
/// into the current thread (ADR-0008). Resolution: exact id → unique
/// id-prefix → unique title substring; multiple hits list candidates.
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
/// Session Mapping activation, the one Session Snapshot receipt with its
/// claimable pendings, the owner check, and `--force` to steal a mapping owned
/// by another chat. The parent stays mapped and switchable; no prompt is ever
/// sent into the child (ADR-0054).
async fn handle_sub_attach(
    handles: &CommandHandles,
    thread_key: &ThreadKey,
    query: &str,
    force: bool,
    message_id: &str,
    kind: ConversationKind,
) -> crate::error::Result<()> {
    let Some(active) = handles.flow.sessions.active_entry(thread_key).await else {
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
    let active_id = active.session_id.as_str();
    let sessions = handles
        .flow
        .sessions
        .cached_session_list(&handles.flow.backend)
        .await?;
    let archived = |s: &&crate::opencode::types::SessionListInfo| {
        s.time.as_ref().map(|t| t.is_archived()).unwrap_or(false)
    };
    let child_of_active = |s: &&crate::opencode::types::SessionListInfo| {
        s.parent_id.as_deref() == Some(active_id) && !archived(s)
    };
    let children: Vec<crate::opencode::types::SessionListInfo> =
        sessions.iter().filter(child_of_active).cloned().collect();
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
            // list, so re-running `/sub attach` on it must be caught here. It
            // goes through the same adoption path, whose already-active branch
            // writes nothing and sends no snapshot.
            let active_info = sessions
                .iter()
                .find(|s| s.id == active_id)
                .cloned()
                .unwrap_or_else(|| crate::opencode::types::SessionListInfo {
                    id: active.session_id.clone(),
                    title: crate::bridge::display::id_tail(&active.session_id),
                    directory: active.directory.clone(),
                    parent_id: None,
                    agent: None,
                    model: None,
                    time: None,
                });
            if matches!(
                resolve_session(std::slice::from_ref(&active_info), query),
                SessionResolution::Hit(_)
            ) {
                return adopt_session(
                    handles,
                    thread_key,
                    &active_info,
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
            let out_of_scope: Vec<crate::opencode::types::SessionListInfo> = sessions
                .iter()
                .filter(|s| !child_of_active(s) && !archived(s))
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
    let owner = {
        let store = handles.flow.sessions.store.lock().await;
        store.thread_for_session(&info.id)
    };
    if let Some(owner_key) = owner
        && owner_key != *thread_key
    {
        if !force {
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
        // --force: steal the mapping; the other thread becomes sessionless.
        handles.flow.sessions.remove_session(&info.id).await?;
    }
    // Open the topic in one transaction (ADR-0016): the pre-mapping snapshot,
    // cover card (ADR-0023), in-topic seed, Session Mapping and the snapshot
    // claim all live in `bridge::topic`.
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
/// The adoption confirmation is the Session Snapshot card (ADR-0028): in a
/// never-had-a-session topic it is sent inside the topic and doubles as the
/// fallback-card anchor (`reply_card_in_thread`, ADR-0006); in the lobby it
/// is the reply replacing the old 「已接管…」 text.
///
/// `force_hint` is the command the owner refusal points the user at (with
/// `--force` appended): `/switch` for the general adoption path, `/sub attach`
/// for the child-takeover path, so a refusal never advertises a route the
/// caller's own policy would refuse.
async fn adopt_session(
    handles: &CommandHandles,
    thread_key: &ThreadKey,
    info: &crate::opencode::types::SessionListInfo,
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
    if let Some(owner_key) = owner
        && owner_key != *thread_key
    {
        if !force {
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
        // --force: steal the mapping; the other thread becomes sessionless.
        handles.flow.sessions.remove_session(&info.id).await?;
    }

    // ADR-0028: every adoption ends in exactly ONE Session Snapshot card
    // (replacing the old 「已接管…」 texts) — never a card plus a text, never
    // nothing. Gathered BEFORE the mapping write below, so the card reflects
    // the session's pre-adoption state; each field is best-effort, so a read
    // failure degrades that field rather than blocking the adoption.
    let (card, data) =
        crate::bridge::snapshot::snapshot_card_for(&handles.snapshot_handles(), "接管", info, None).await;

    let anchor = if kind == ConversationKind::Topic {
        // The snapshot is sent inside the topic and doubles as the fallback-card
        // anchor (ADR-0023): permission/question cards reply to it and land
        // inside the topic. Its message id is persisted as `topic_anchor`.
        match handles
            .flow
            .platform
            .reply_card_in_thread(message_id, &card)
            .await
        {
            Ok((anchor, _)) => Some(anchor),
            Err(e) => {
                tracing::warn!("attach: snapshot in-thread send failed: {}", e);
                None
            }
        }
    } else {
        None
    };
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
    entry.topic_anchor = anchor.clone();
    entry.topic_root = pending_topic_root;
    handles.flow.sessions.activate(entry).await?;
    crate::bridge::topic::claim_pending_cover(&handles.topic_handles(), thread_key, &info.id).await;
    // In a topic the snapshot was already sent inside it (the in-thread send
    // above); don't reply twice.
    if kind != ConversationKind::Topic {
        let mid = handles.flow.platform.reply_card(message_id, &card).await?;
        crate::bridge::external::settle_snapshot_after_send(
            &handles.external,
            &handles.flow,
            &mid,
            "接管",
            &info.title,
            &data,
            None,
        )
        .await;
    } else if let Some(anchor) = &anchor {
        crate::bridge::external::settle_snapshot_after_send(
            &handles.external,
            &handles.flow,
            anchor,
            "接管",
            &info.title,
            &data,
            None,
        )
        .await;
    }
    Ok(())
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
/// (used by `/switch <keyword>` and the `/switch` card's filtered list).
/// `/attach` and `/topic --adopt` do NOT use this —
/// they resolve via `resolve_session` (exact id → id-prefix → whole-title
/// substring) so adoption stays unambiguous.
///
/// `lower` is split on whitespace (spaces AND newlines) and every token must
/// hit somewhere in the session's title/dir/id — an AND over tokens. This makes
/// the multiline `/switch` search box usable: pasted/typed multi-line input is
/// searched as its words, not as one literal string (which could never match).
pub(crate) fn matches_keyword(s: &crate::opencode::types::SessionListInfo, lower: &str) -> bool {
    let haystack = format!(
        "{} {} {}",
        s.title.to_lowercase(),
        s.directory.to_lowercase(),
        s.id.to_lowercase()
    );
    lower.split_whitespace().all(|tok| haystack.contains(tok))
}

/// The "ambiguous match" candidate list shown when a keyword or id resolves to
/// several sessions: `title · dir · id-tail`, capped at 8.
fn candidates_list(header: &str, sessions: &[&crate::opencode::types::SessionListInfo]) -> String {
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

    fn session(id: &str, title: &str) -> crate::opencode::types::SessionListInfo {
        crate::opencode::types::SessionListInfo {
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
        assert!(command_help("think").unwrap().contains("/think"));
        assert!(command_help("dir").unwrap().contains("next non-command message"));
        assert!(command_help("version").unwrap().contains("/version"));
        assert!(command_help("sub").unwrap().contains("/sub list"));
        assert!(
            command_help("sub").unwrap().contains("/sub attach"),
            "the sub help documents the takeover form"
        );
        assert_eq!(command_help("nonexistent"), None);
        // The retired text-list form has no help topic of its own.
        assert_eq!(command_help("list"), None);
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
}
