//! Errors the Codex adapter raises while attaching (spec #860, ticket #876).
//!
//! They are deliberately separate from `crate::error::BridgeError`: this slice
//! is the adapter's own construction-time vocabulary, and none of it is a
//! bridge concern yet. A caller above the adapter maps a construction failure
//! to its own error when the adapter is wired into the process (tickets
//! #877/#879).

use std::time::Duration;

use thiserror::Error;

pub(crate) type Result<T> = std::result::Result<T, CodexError>;

#[derive(Debug, Error)]
pub(crate) enum CodexError {
    /// The listen URL is not one this client speaks. Codex also supports
    /// `stdio://`, which cola deliberately does not: a peer attachment cannot
    /// own the agent's stdin/stdout.
    #[error(
        "codex app-server endpoint `{0}` is not supported; this client speaks `ws://HOST:PORT` \
         and `unix://PATH` (not stdio)"
    )]
    Endpoint(String),

    /// A transport-level failure: the socket did not accept, the WebSocket
    /// handshake failed, or a frame was not valid JSON.
    #[error("{context}: {detail}")]
    Transport { context: String, detail: String },

    /// The endpoint is a unix-domain socket, and this build's platform does
    /// not provide one. Windows has AF_UNIX limits (ADR-0090's open question
    /// 4), so the failure is named rather than surfacing as an obscure IO
    /// error.
    #[error(
        "codex app-server endpoint `unix://{path}` needs a unix domain socket, which this build \
         does not support on {platform}; use `ws://HOST:PORT` instead"
    )]
    UnsupportedPlatform { path: String, platform: &'static str },

    /// The connection or handshake did not answer inside the budget. A wedged
    /// server must not hold cola's boot.
    #[error("codex app-server did not answer within {0:?}")]
    Timeout(Duration),

    /// A JSON-RPC call answered with the protocol's `error` member.
    #[error("codex app-server `{method}` failed: {detail}")]
    Rpc { method: String, detail: String },

    /// The peer closed or sent no frame where a response was required.
    #[error("codex app-server closed the connection before answering `{0}`")]
    NoResponse(String),

    /// A frame was valid JSON but not the JSON-RPC shape the call expected.
    #[error("codex app-server reply to `{method}` is not valid JSON-RPC: {detail}")]
    Protocol { method: String, detail: String },

    /// `initialize`'s result is the only place the server identifies itself.
    #[error(
        "codex app-server `initialize` result carries no `userAgent`; the adapter cannot identify \
         the binary it is attached to"
    )]
    MissingUserAgent,

    /// `userAgent` did not carry a `codex-*` product with a semantic version.
    #[error("codex app-server `initialize` result has an unparseable `userAgent`: `{0}`")]
    UnparseableUserAgent(String),

    /// The server is older than the pinned surface. An older binary may lack
    /// fields the adapter reads, so it is refused instead of guessed at.
    #[error(
        "codex app-server reports {found}, but this adapter is written against codex-cli {pinned}; \
         an older binary is refused — upgrade the Codex daemon"
    )]
    OlderServer { pinned: String, found: String },

    /// The probe could not describe the server's surface. Never guessed: the
    /// adapter refuses instead of assuming the pinned surface still applies.
    #[error("could not probe the Codex schema: {0}")]
    Probe(String),

    /// The binary used for the schema probe is not the binary the server runs.
    /// A 0.163 CLI's schema says nothing about a 0.164 daemon.
    #[error(
        "codex binary `{binary}` reports {binary_version}, but the running app-server reports \
         {server_version}; probing a different binary would not describe the running server"
    )]
    BinaryVersionMismatch {
        binary: String,
        binary_version: String,
        server_version: String,
    },

    /// The server's surface lacks a method this adapter needs. Construction
    /// fails here — with the method named — never at the first prompt.
    #[error(
        "codex app-server {version} does not provide the method(s) {missing}, which this adapter \
         requires (contract pinned to codex-cli {pinned})"
    )]
    MissingContract {
        version: String,
        missing: String,
        pinned: String,
    },

    /// No shared `CODEX_HOME` can be resolved: neither the variable nor a home
    /// directory is set, or the configured value is relative (Codex requires an
    /// absolute path, and cola must read the same store as the user's client).
    #[error("cannot locate the shared CODEX_HOME: {0}")]
    CodexHome(String),

    /// A management-command failure (`codex app-server daemon …`): the CLI
    /// could not be run, did not return within its bound, or refused. This is
    /// the CLI's own surface, not the app-server protocol.
    #[error("codex daemon command failed: {0}")]
    Daemon(String),

    /// The daemon's control path exists but is not something this client can
    /// dial (not a unix socket, or unreadable). Absence is a state;
    /// this is a misconfiguration and fails loudly.
    #[error("the codex daemon control socket at `{socket}` is unusable: {detail}")]
    DaemonSocket { socket: String, detail: String },
}
