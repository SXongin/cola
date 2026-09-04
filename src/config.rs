use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub fn load(path: &Path) -> anyhow::Result<Config> {
    let content = std::fs::read_to_string(path)?;
    Ok(toml::from_str(&content)?)
}

/// Resolve the config file path using the fallback lookup.
///
/// Order (first existing wins):
/// 1. `explicit` — the user's `--config` flag, used verbatim (missing explicit
///    file is a hard error, never silently ignored).
/// 2. `./cola.toml` in the current directory.
/// 3. `~/.cola/cola.toml` — the same cross-platform dir as logs, session
///    mapping, lock and pid files, so launcher shortcuts without a notion of
///    "current directory" still find it.
///
/// Returns `None` when no fallback file exists.
pub fn resolve_config_path(explicit: Option<&str>, cwd: &Path, home: &Path) -> Option<PathBuf> {
    if let Some(path) = explicit {
        return Some(PathBuf::from(path));
    }
    let cwd_config = cwd.join("cola.toml");
    if cwd_config.exists() {
        return Some(cwd_config);
    }
    let home_config = home.join(".cola").join("cola.toml");
    if home_config.exists() {
        return Some(home_config);
    }
    None
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub opencode: OpenCodeConfig,
    pub feishu: FeishuConfig,
    #[serde(default)]
    pub bridge: BridgeConfig,
}

/// When cola starts an `opencode serve` of its own (ADR-0013).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ServerStartPolicy {
    /// Lazy: attach at boot, spawn an Owned Server only at the moment a prompt
    /// actually needs one and discovery finds no server. The default.
    #[default]
    Auto,
    /// Attach-only: cola never spawns an Owned Server. If no server exists the
    /// bot replies that OpenCode is unavailable.
    Never,
    /// The old behavior: spawn an Owned Server at boot when none is running.
    Eager,
}

impl ServerStartPolicy {
    /// Whether a demand-time lazy spawn is allowed (`Never` forbids it).
    pub fn spawns_when_needed(self) -> bool {
        !matches!(self, Self::Never)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenCodeConfig {
    /// Preferred/fallback port. Optional: discovery rewrites the effective
    /// endpoint to whatever server it finds on the shared store, so an absent
    /// `url` just means "use the default `http://localhost:4096`". In a
    /// coexistence setup it is only a tiebreaker among servers of the same
    /// class (ADR-0013).
    #[serde(default)]
    pub url: Option<String>,
    /// Default model for new sessions, "provider/model". When unset, cola does
    /// NOT pin a model — the OpenCode server uses its own default model (the
    /// server falls back to `provider.defaultModel()`), so "不写 model" just
    /// means "用 opencode 的默认模型".
    #[serde(default)]
    pub model: Option<String>,
    /// When cola may start its own `opencode serve` (ADR-0013): `auto` (lazy,
    /// the default), `never` (attach-only), `eager` (spawn at boot).
    #[serde(default)]
    pub start_server: ServerStartPolicy,
}

impl Default for OpenCodeConfig {
    fn default() -> Self {
        Self {
            url: None,
            model: None,
            start_server: ServerStartPolicy::Auto,
        }
    }
}

impl OpenCodeConfig {
    /// The preferred port from `[opencode] url`, if parseable. A tiebreaker in
    /// `pick_server` among servers of the same class (ADR-0013), never an
    /// absolute pin.
    pub fn preferred_port(&self) -> Option<u16> {
        self.url
            .as_deref()
            .and_then(|u| u.rsplit(':').next())
            .and_then(|s| s.parse().ok())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeishuConfig {
    pub app_id: String,
    pub app_secret: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BridgeConfig {
    #[serde(default = "default_session_file")]
    pub session_file: PathBuf,
    /// Default directory for new sessions. When unset, falls back to the
    /// process working directory. `/dir` overrides per session.
    #[serde(default)]
    pub work_dir: Option<PathBuf>,
    /// In group chats, reply to the requester's message with a short
    /// completion notice (the streaming card is patched in place and so does
    /// not push a new notification). p2p chats don't need it.
    #[serde(default = "default_group_completion_notice")]
    pub group_completion_notice: bool,
    /// How many days of rotated daily logs to keep (default 14). Older
    /// `cola-YYYY-MM-DD.log` files are swept on startup and at each rotation.
    #[serde(default = "default_log_days")]
    pub log_days: u32,
}

impl Default for BridgeConfig {
    fn default() -> Self {
        Self {
            session_file: default_session_file(),
            work_dir: None,
            group_completion_notice: default_group_completion_notice(),
            log_days: default_log_days(),
        }
    }
}

fn default_group_completion_notice() -> bool {
    true
}

fn default_log_days() -> u32 {
    14
}

fn default_session_file() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".cola")
        .join("sessions.json")
}

/// A key that uniquely identifies a session context on the Feishu side.
///
/// `thread_id` is the authoritative topic identifier (`omt_...`): present on
/// topic messages (in groups or p2p). For non-topic messages (a group-root or
/// a p2p top-level message) it falls back to the `chat_id`, which makes the
/// whole chat a single "lobby" conversation.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ThreadKey {
    pub chat_id: String,
    #[serde(alias = "root_id")]
    pub thread_id: String,
}

impl ThreadKey {
    pub fn new(chat_id: String, thread_id: String) -> Self {
        Self { chat_id, thread_id }
    }

    /// Derive the conversation key for an incoming message.
    ///
    /// A message that carries a `thread_id` is a topic message → its own key.
    /// Anything else (no `thread_id`) is the chat's top level: a single
    /// "lobby" conversation keyed by `chat_id` itself.
    pub fn from_message(chat_id: &str, thread_id: Option<&str>) -> Self {
        match thread_id {
            Some(tid) if !tid.is_empty() => Self::new(chat_id.to_string(), tid.to_string()),
            _ => Self::new(chat_id.to_string(), chat_id.to_string()),
        }
    }
}

/// How an incoming message maps to a conversation. Encapsulates the
/// group-root/lobby policy: p2p top-level messages are normal conversations;
/// group-root messages are a lobby (with guidance); topic messages (any chat
/// type) are isolated conversations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConversationKind {
    /// A message inside a topic (group or p2p) → topic-isolated session.
    Topic,
    /// A top-level message in a group → the group's lobby session.
    GroupLobby,
    /// A top-level p2p message → the p2p conversation.
    P2p,
}

impl ConversationKind {
    /// Classify an incoming message. `thread_id` is `Some` iff the message is
    /// inside a topic (findings: a message is a topic message IFF it has one).
    pub fn classify(chat_type: &str, thread_id: Option<&str>) -> Self {
        let has_thread = thread_id.map(|t| !t.is_empty()).unwrap_or(false);
        if has_thread {
            ConversationKind::Topic
        } else if chat_type == "p2p" {
            ConversationKind::P2p
        } else {
            ConversationKind::GroupLobby
        }
    }

    /// The thread key routing a message of this kind.
    pub fn thread_key(&self, chat_id: &str, thread_id: Option<&str>) -> ThreadKey {
        match self {
            ConversationKind::Topic => ThreadKey::from_message(chat_id, thread_id),
            // Non-topic kinds are always the chat's top level: the key is the
            // chat itself, regardless of any stray thread_id.
            _ => ThreadKey::new(chat_id.to_string(), chat_id.to_string()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEntry {
    pub thread_key: ThreadKey,
    pub session_id: String,
    pub directory: String,
    /// Per-session agent override set by `/agent`, sent as a per-prompt
    /// `PromptInput.agent` on the next message. Persisted so it survives a cola
    /// restart; when `None` the server's default agent applies.
    #[serde(default)]
    pub agent: Option<String>,
    /// Per-session model override ("provider/model") set by `/model`, sent as a
    /// per-prompt `PromptInput.model` on the next message. Persisted so it
    /// survives a cola restart; when `None` the configured default (or the
    /// server's own default) applies.
    #[serde(default)]
    pub model: Option<String>,
    /// Per-session thinking level ("variant", e.g. "high") set by `/think`,
    /// sent as a per-prompt `PromptInput.variant` on the next message. A model
    /// declares its own variant set — there is no universal scale (ADR-0020);
    /// `None` means the server's default for whatever model runs this turn.
    /// Persisted; cleared automatically when `/model` switches to a model that
    /// doesn't declare the current variant.
    #[serde(default)]
    pub variant: Option<String>,
    /// When true, pending permission requests for this session are answered
    /// automatically (`/autoaccept`) instead of showing a Feishu card.
    #[serde(default)]
    pub auto_accept: bool,
    /// For topic-backed sessions: the `message_id` of a message INSIDE the
    /// topic (recorded when the topic was created via `/topic`). Permission /
    /// question / external cards that must be *sent* (no in-flight card to
    /// reply to) reply to this anchor, which keeps them inside the topic — the
    /// create API rejects `receive_id_type=thread_id`.
    #[serde(default)]
    pub topic_anchor: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_message_uses_thread_id_when_present() {
        let key = ThreadKey::from_message("oc_group_1", Some("omt_topic_1"));
        assert_eq!(key.chat_id, "oc_group_1");
        assert_eq!(key.thread_id, "omt_topic_1");
        assert_ne!(key.thread_id, key.chat_id);
    }

    #[test]
    fn from_message_falls_back_to_chat_id_for_non_topic() {
        let key = ThreadKey::from_message("oc_group_1", None);
        assert_eq!(key.thread_id, "oc_group_1");

        let empty = ThreadKey::from_message("oc_p2p_1", Some(""));
        assert_eq!(empty.thread_id, "oc_p2p_1");
    }

    #[test]
    fn classify_topic_messages_in_group_and_p2p() {
        assert_eq!(
            ConversationKind::classify("group", Some("omt_t_1")),
            ConversationKind::Topic
        );
        assert_eq!(
            ConversationKind::classify("p2p", Some("omt_t_1")),
            ConversationKind::Topic
        );
    }

    #[test]
    fn classify_group_root_is_lobby_p2p_top_is_conversation() {
        assert_eq!(
            ConversationKind::classify("group", None),
            ConversationKind::GroupLobby
        );
        assert_eq!(ConversationKind::classify("p2p", None), ConversationKind::P2p);
    }

    #[test]
    fn classify_topic_key_keeps_thread_id_others_use_chat() {
        let topic = ConversationKind::Topic.thread_key("oc_g1", Some("omt_t_1"));
        assert_eq!(topic.thread_id, "omt_t_1");

        let lobby = ConversationKind::GroupLobby.thread_key("oc_g1", Some("omt_t_1"));
        assert_eq!(lobby.thread_id, "oc_g1");

        let p2p = ConversationKind::P2p.thread_key("oc_p1", None);
        assert_eq!(p2p.thread_id, "oc_p1");
    }

    #[test]
    fn deserializes_legacy_root_id_field() {
        // Old sessions.json entries used `root_id`; the serde alias keeps them loadable.
        let key: ThreadKey = serde_json::from_str(r#"{"chat_id":"oc_1","root_id":"omt_old"}"#).unwrap();
        assert_eq!(key.thread_id, "omt_old");
    }

    #[test]
    fn start_server_policy_deserializes_three_states() {
        let parse = |s: &str| toml::from_str::<OpenCodeConfig>(s).unwrap().start_server;
        assert_eq!(parse(r#"start_server = "auto""#), ServerStartPolicy::Auto);
        assert_eq!(parse(r#"start_server = "never""#), ServerStartPolicy::Never);
        assert_eq!(parse(r#"start_server = "eager""#), ServerStartPolicy::Eager);
    }

    #[test]
    fn start_server_defaults_to_auto() {
        let cfg: OpenCodeConfig = toml::from_str("").unwrap();
        assert_eq!(cfg.start_server, ServerStartPolicy::Auto);
        assert!(cfg.start_server.spawns_when_needed());
        assert!(!ServerStartPolicy::Never.spawns_when_needed());
        assert!(ServerStartPolicy::Eager.spawns_when_needed());
    }

    #[test]
    fn feishu_only_config_loads() {
        // README's minimal config: only the Feishu credentials. `opencode` must
        // default rather than fail with "missing field".
        let toml_str = r#"
            [feishu]
            app_id = "cli_test"
            app_secret = "secret"
        "#;
        let cfg: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.opencode.start_server, ServerStartPolicy::Auto);
        assert!(cfg.opencode.url.is_none());
        assert!(cfg.opencode.model.is_none());
        assert_eq!(cfg.feishu.app_id, "cli_test");
        assert_eq!(cfg.feishu.app_secret, "secret");
    }

    #[test]
    fn feishu_plus_opencode_model_config_loads() {
        // The historical issue #02 criterion: `[feishu]` + an `[opencode]` table
        // holding only `model` — the other `[opencode]` fields must default.
        let toml_str = r#"
            [feishu]
            app_id = "cli_test"
            app_secret = "secret"

            [opencode]
            model = "opencode/deepseek-v4-flash"
        "#;
        let cfg: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.opencode.model.as_deref(), Some("opencode/deepseek-v4-flash"));
        assert!(cfg.opencode.url.is_none());
        assert_eq!(cfg.opencode.start_server, ServerStartPolicy::Auto);
        assert_eq!(cfg.feishu.app_id, "cli_test");
    }

    #[test]
    fn resolve_config_explicit_wins_even_if_missing() {
        // An explicit --config is used verbatim; existence is not checked here
        // (a missing explicit file is a hard error upstream, never a fallback).
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        std::fs::write(cwd.path().join("cola.toml"), "").unwrap();
        let p = resolve_config_path(Some("/custom/cola.toml"), cwd.path(), home.path());
        assert_eq!(p.unwrap(), PathBuf::from("/custom/cola.toml"));
    }

    #[test]
    fn resolve_config_prefers_cwd_over_home() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        std::fs::write(cwd.path().join("cola.toml"), "").unwrap();
        std::fs::create_dir_all(home.path().join(".cola")).unwrap();
        std::fs::write(home.path().join(".cola/cola.toml"), "").unwrap();
        let p = resolve_config_path(None, cwd.path(), home.path());
        assert_eq!(p.unwrap(), cwd.path().join("cola.toml"));
    }

    #[test]
    fn resolve_config_falls_back_to_home_when_cwd_missing() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".cola")).unwrap();
        std::fs::write(home.path().join(".cola/cola.toml"), "").unwrap();
        let p = resolve_config_path(None, cwd.path(), home.path());
        assert_eq!(p.unwrap(), home.path().join(".cola/cola.toml"));
    }

    #[test]
    fn resolve_config_none_when_nothing_exists() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        assert_eq!(resolve_config_path(None, cwd.path(), home.path()), None);
    }
}
