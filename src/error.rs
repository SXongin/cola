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

impl BridgeError {
    /// True when the OpenCode server reported the session does not exist
    /// (404, or the dedicated SessionNotFound variant). cola recreates the
    /// session and retries once in that case — this happens when the mapped
    /// session lives in a different (old) store after a store/server switch.
    pub fn is_session_not_found(&self) -> bool {
        match self {
            BridgeError::SessionNotFound(_) => true,
            BridgeError::Http(e) => e.status() == Some(reqwest::StatusCode::NOT_FOUND),
            _ => false,
        }
    }
}
