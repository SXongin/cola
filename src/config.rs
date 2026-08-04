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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BridgeConfig {
    #[serde(default = "default_session_file")]
    pub session_file: PathBuf,
}

fn default_session_file() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".cola")
        .join("sessions.json")
}

/// A key that uniquely identifies a session context on the Feishu side.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ThreadKey {
    pub chat_id: String,
    pub root_id: String,
}

impl ThreadKey {
    pub fn new(chat_id: String, root_id: String) -> Self {
        Self { chat_id, root_id }
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
}
