use thiserror::Error;

#[derive(Debug, Error)]
#[allow(dead_code)] // variants reachable as subsystems expand
pub enum BridgeError {
    #[error("opencode error: {0}")]
    OpenCode(String),

    #[error("feishu error: {0}")]
    Feishu(String),

    #[error("session not found: {0}")]
    SessionNotFound(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("url parse error: {0}")]
    Url(#[from] url::ParseError),
}

pub type Result<T> = std::result::Result<T, BridgeError>;
