//! The generation-blind HTTP transport shared by the adapter and its
//! generation strategies (ADR-0055).
//!
//! Base URL, credentials, the reqwest client, the reply-endpoint policy and the
//! shared read diagnostics (`read_failure` / `body_preview`) live here; nothing
//! protocol-generation-specific does. The server cola talks to can be
//! restarted/replaced at runtime (another tool like OpenChamber manages it),
//! which may change its port and password, so the live endpoint is held behind
//! a lock and can be swapped wholesale on reconnect.

use base64::Engine;
use std::sync::{Arc, RwLock};

/// Total timeout for the request-reply endpoints (permission/question). They
/// resolve an already-pending request and answer in milliseconds when healthy,
/// so a hung server must not hold the card-callback handler past Feishu's 3 s
/// ack budget. Prompt POSTs deliberately keep the transport's no-total-timeout
/// policy: real turns run for minutes.
pub(crate) const REPLY_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(1500);

/// How much of a failed response body a diagnostic carries. One cap for every
/// read on both generations, so no two diagnostics of the same response
/// truncate differently; a body is a debug aid, not data.
pub(crate) const BODY_PREVIEW_CHARS: usize = 500;

/// The first [`BODY_PREVIEW_CHARS`] characters of a failed response body, for a
/// diagnostic. Char-based, so truncating a multi-byte body can never split a
/// character and panic.
pub(crate) fn body_preview(body: &str) -> &str {
    match body.char_indices().nth(BODY_PREVIEW_CHARS) {
        Some((index, _)) => &body[..index],
        None => body,
    }
}

/// Consume a failed read response into the neutral error: one WARN naming the
/// operation and the status with a bounded body preview, and the
/// [`crate::error::BridgeError::OpenCode`] the caller returns. Both
/// generations' list/status reads share it, so their diagnostics cannot drift
/// (and none of them silently writes to stderr). The returned error names the
/// operation and the status only — its callers log it and surface a fixed card
/// — so use [`read_failure_detailed`] when the error text itself is the
/// diagnostic.
pub(crate) async fn read_failure(response: reqwest::Response, what: &str) -> crate::error::BridgeError {
    let (status, _) = consume_failure(response, what).await;
    crate::error::BridgeError::OpenCode(format!("{what} failed: {status}"))
}

/// [`read_failure`] with the bounded body preview in the RETURNED error too,
/// for a read whose failure is surfaced rather than merely logged — the prompt
/// polyfill's status poll, whose error propagates out of `prompt` and is the
/// only diagnostic the caller gets. Same WARN either way.
pub(crate) async fn read_failure_detailed(
    response: reqwest::Response,
    what: &str,
) -> crate::error::BridgeError {
    let (status, body) = consume_failure(response, what).await;
    crate::error::BridgeError::OpenCode(format!("{what} failed: {status} — body: {}", body_preview(&body)))
}

/// The shared failure consumption: read the status and body once, WARN with the
/// bounded preview, and hand both back so the two wrappers above cannot log
/// differently.
async fn consume_failure(response: reqwest::Response, what: &str) -> (reqwest::StatusCode, String) {
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    tracing::warn!("{what} failed: {status} — body: {}", body_preview(&body));
    (status, body)
}

/// The live endpoint: the reqwest client (with its baked-in auth headers) and
/// the base URL. Shared behind an `Arc` so [`Transport::repoint`] can replace
/// it without dropping a backend handle.
#[derive(Clone)]
pub(crate) struct Transport {
    state: Arc<RwLock<HttpState>>,
}

struct HttpState {
    client: reqwest::Client,
    base_url: String,
}

impl Transport {
    /// A transport bound to a server, or serverless (empty `base_url`) when
    /// Lazy Start hasn't spawned one yet (ADR-0013). A serverless transport
    /// does no requests until `repoint` points it at a real server; the
    /// username is still pinned so a later reconnect carries Basic auth with
    /// both parts.
    pub(crate) fn new(username: Option<&str>, password: Option<&str>, base_url: impl Into<String>) -> Self {
        Self::new_with(username, password, base_url, false)
    }

    /// The shared builder. `no_env_proxy` is set only by wire tests: their fake
    /// server is loopback, and a developer shell that exports `http_proxy` must
    /// not intercept it — production keeps honoring env proxies (ticket 09).
    fn new_with(
        username: Option<&str>,
        password: Option<&str>,
        base_url: impl Into<String>,
        no_env_proxy: bool,
    ) -> Self {
        Self {
            state: Arc::new(RwLock::new(HttpState {
                client: build_http_client_with(username, password, no_env_proxy),
                base_url: base_url.into().trim_end_matches('/').to_string(),
            })),
        }
    }

    /// The current base URL.
    pub(crate) fn base_url(&self) -> String {
        self.state.read().unwrap().base_url.clone()
    }

    /// The base URL with a relative path appended.
    pub(crate) fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url(), path)
    }

    /// The live reqwest client (a cheap clone of the shared handle).
    pub(crate) fn client(&self) -> reqwest::Client {
        self.state.read().unwrap().client.clone()
    }

    /// Point the transport at a different server (port/password changed because
    /// the old one was restarted/replaced). Rare; only called by the adapter's
    /// reconnect loop when discovery finds the attached server is gone.
    pub(crate) fn repoint(&self, url: &str, password: &str, username: Option<&str>) {
        let state = HttpState {
            client: build_http_client(username, Some(password)),
            base_url: url.trim_end_matches('/').to_string(),
        };
        let mut w = self.state.write().unwrap();
        *w = state;
    }

    /// Test-only: replace the client with a no-proxy one so the wire tests'
    /// loopback fake server cannot be intercepted by a developer shell's
    /// `http_proxy` (ticket 09). Every other byte of the construction stays
    /// production's (ADR-0031).
    #[cfg(test)]
    pub(crate) fn disable_env_proxy(&self, username: Option<&str>, password: Option<&str>) {
        self.state.write().unwrap().client = build_http_client_with(username, password, true);
    }
}

/// Build a reqwest client with the standard JSON content-type and optional
/// Basic auth (OpenCode server password).
fn build_http_client(username: Option<&str>, password: Option<&str>) -> reqwest::Client {
    build_http_client_with(username, password, false)
}

/// The transport builder. Basic auth is attached only when BOTH username and
/// password are present — a half-credential must never reach the wire (the
/// server checks the username too, so a password-only request 401s).
fn build_http_client_with(username: Option<&str>, password: Option<&str>, no_proxy: bool) -> reqwest::Client {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(reqwest::header::CONTENT_TYPE, "application/json".parse().unwrap());

    let mut builder = reqwest::Client::builder()
        .default_headers(headers)
        // Bound TCP connect only — NOT a total request timeout. A prompt POST
        // legitimately runs for many minutes, and a total timeout would abort
        // real turns. Long waits on an established connection are bounded by
        // the callers that can afford to give up (e.g. the request poller).
        .connect_timeout(std::time::Duration::from_secs(10));

    if no_proxy {
        builder = builder.no_proxy();
    }

    if let (Some(user), Some(pass)) = (username, password) {
        let auth = format!("{}:{}", user, pass);
        let encoded = base64::engine::general_purpose::STANDARD.encode(auth);
        let auth_value = reqwest::header::HeaderValue::from_str(&format!("Basic {}", encoded)).unwrap();
        let mut default_headers = reqwest::header::HeaderMap::new();
        default_headers.insert(reqwest::header::AUTHORIZATION, auth_value);
        builder = builder.default_headers(default_headers);
    }

    builder.build().expect("failed to build reqwest client")
}
