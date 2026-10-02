use thiserror::Error;

#[derive(Debug, Error)]
#[allow(dead_code)] // variants reachable as subsystems expand
pub enum BridgeError {
    #[error("opencode error: {0}")]
    OpenCode(String),

    #[error("feishu error: {0}")]
    Feishu(String),

    /// A Feishu REST call refused with a non-success HTTP status, the status
    /// typed so a failed card write can tell a recoverable 5xx/429/408 from a
    /// permanent 4xx (ADR-0067's Pending Card Update classification). The
    /// display text matches the `Feishu` variant's, so log lines and messages
    /// do not change shape.
    #[error("feishu error: {detail}")]
    FeishuHttp { status: u16, detail: String },

    /// Feishu rejected the CARD CONTENT itself (HTTP 400 `code: 230099`,
    /// "Failed to create card content"): the platform's parser or a card limit
    /// refused the JSON, so re-sending the same content never succeeds. The
    /// render layer degrades the card (fenced markdown) instead of retrying it
    /// verbatim, which would leave the turn's card frozen forever.
    #[error("card content rejected ({code}): {detail}")]
    CardContentRejected { code: i64, detail: String },

    #[error("session not found: {0}")]
    SessionNotFound(String),

    /// The addressed request/resource no longer exists on the backend (a
    /// request-reply endpoint returned 404). Distinct from `SessionNotFound`:
    /// this is the benign "already resolved elsewhere" case, not a stale
    /// session mapping.
    #[error("not found: {0}")]
    NotFound(String),

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

    /// True when the backend reported the addressed request/resource no longer
    /// exists — a 404, or the dedicated `NotFound` variant a request-reply
    /// endpoint maps one to. A resolved-elsewhere permission/question is a
    /// benign outcome, not a failure.
    pub fn is_not_found(&self) -> bool {
        match self {
            BridgeError::NotFound(_) => true,
            BridgeError::Http(e) => e.status() == Some(reqwest::StatusCode::NOT_FOUND),
            _ => false,
        }
    }

    /// Whether a failed card write may succeed on a retry (ADR-0067): transport
    /// errors, timeouts, 5xx, 429 and 408 are recoverable; a content rejection
    /// or any other refusal is permanent. `Feishu(String)` covers API-level
    /// errors reported inside a 2xx body — a semantic refusal, never retried.
    pub(crate) fn is_recoverable_card_write(&self) -> bool {
        match self {
            BridgeError::Http(_) | BridgeError::Io(_) => true,
            BridgeError::FeishuHttp { status, .. } => *status >= 500 || *status == 429 || *status == 408,
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR-0067's retry rule: transport, timeouts, 5xx and rate limits recover;
    /// a refused content or any other refusal is permanent.
    #[test]
    fn card_write_recoverability_follows_the_retry_rule() {
        assert!(BridgeError::Io(std::io::Error::other("connection reset")).is_recoverable_card_write());

        for status in [500u16, 502, 503, 429, 408] {
            assert!(
                BridgeError::FeishuHttp {
                    status,
                    detail: String::new()
                }
                .is_recoverable_card_write(),
                "{status} must be retryable"
            );
        }
        for status in [400u16, 401, 403, 404, 422] {
            assert!(
                !BridgeError::FeishuHttp {
                    status,
                    detail: String::new()
                }
                .is_recoverable_card_write(),
                "{status} must be permanent"
            );
        }

        assert!(
            !BridgeError::CardContentRejected {
                code: 230099,
                detail: String::new()
            }
            .is_recoverable_card_write()
        );
        assert!(
            !BridgeError::Feishu("update error 230002: message not found".into()).is_recoverable_card_write()
        );
    }
}
