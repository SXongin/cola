use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub fn load(path: &str) -> anyhow::Result<Config> {
    let content = std::fs::read_to_string(path)?;
    Ok(toml::from_str(&content)?)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub opencode: OpenCodeConfig,
    pub feishu: FeishuConfig,
    #[serde(default)]
    pub bridge: BridgeConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenCodeConfig {
    pub url: String,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default = "default_model")]
    pub model: String,
}

fn default_model() -> String {
    "opencode-go/deepseek-v4-flash".to_string()
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
}

impl Default for BridgeConfig {
    fn default() -> Self {
        Self {
            session_file: default_session_file(),
            work_dir: None,
            group_completion_notice: default_group_completion_notice(),
        }
    }
}

fn default_group_completion_notice() -> bool {
    true
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
    pub name: String,
    pub directory: String,
    #[serde(default)]
    pub agent: Option<String>,
    /// When true, pending permission requests for this session are answered
    /// automatically (`/autoaccept`) instead of showing a Feishu card.
    #[serde(default)]
    pub auto_accept: bool,
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
}
