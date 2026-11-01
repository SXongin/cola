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

// `Endpoint` is the parse arm a `ws://` or a malformed listen URL reaches; the
// daemon path never constructs it here.
#[allow(dead_code)]
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

    /// No usable `codex` binary was found to run `app-server proxy`. The
    /// adapter's dial path resolves the binary in a fixed order (the daemon's
    /// reported `managedCodexPath`, the managed package under `CODEX_HOME`,
    /// then the configured CLI / `PATH`); this names every step it tried so a
    /// misconfiguration is actionable rather than a bare spawn failure.
    #[error("cannot find an executable codex binary to run `app-server proxy`; tried: {tried}")]
    ProxyBinary { tried: String },

    /// The resolved proxy binary could not be started. This is a
    /// **configuration** failure, not evidence that the daemon is gone: the
    /// attach path must report it instead of falling through to
    /// `app-server daemon start` (which would launch against a live daemon).
    #[error("could not start the codex app-server proxy `{binary}`: {detail}")]
    ProxySpawn { binary: String, detail: String },

    /// The connection or handshake did not answer inside the budget. A wedged
    /// server must not hold cola's boot.
    #[error("codex app-server did not answer within {0:?}")]
    Timeout(Duration),

    /// A JSON-RPC call answered with the protocol's `error` member. `code` is
    /// kept because the protocol encodes *named states* there — the spike's
    /// `-32600 no rollout found` on a never-run thread is one (ticket #878) —
    /// and matching on a message alone would be guessing.
    #[error("codex app-server `{method}` failed: {detail}")]
    Rpc {
        method: String,
        code: Option<i64>,
        detail: String,
    },

    /// The peer closed or sent no frame where a response was required.
    #[error("codex app-server closed the connection before answering `{0}`")]
    NoResponse(String),

    /// A server-initiated request's response could not enter the writer's
    /// bounded queue inside the budget — the peer stopped reading. The request
    /// is still unanswered, so the caller may retry it.
    #[error(
        "codex app-server response for request {id} could not be queued within {timeout:?}; the request is still unanswered"
    )]
    ResponseTimeout { id: String, timeout: Duration },

    /// A thread subscription could not keep up with the server's push stream:
    /// the broadcast dropped `skipped` events. A silent gap would render a
    /// transcript with holes, so the adapter reports it by name.
    #[error("codex subscription for thread `{thread_id}` fell behind by {skipped} event(s)")]
    SubscriptionLagged { thread_id: String, skipped: u64 },

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

    /// A Windows daemon precondition refused the start before anything was
    /// spawned (ticket #952): an elevated token, a launcher that forbids
    /// breakaway, or a control-socket path over the AF_UNIX budget. The detail
    /// carries the same actionable instruction `cola doctor` prints.
    #[error("cannot start the codex daemon on Windows: {0}")]
    WindowsPrecondition(String),

    /// The daemon's control path exists but is not something this client can
    /// dial (not a unix socket, or unreadable). Absence is a state;
    /// this is a misconfiguration and fails loudly.
    #[error("the codex daemon control socket at `{socket}` is unusable: {detail}")]
    DaemonSocket { socket: String, detail: String },

    /// The resolved control-socket path cannot fit the platform's AF_UNIX
    /// address. On Windows the published path **is** the socket, so unlike
    /// Unix a long `CODEX_HOME` is a precondition failure — checked before
    /// anything is spawned, never left to a connect that cannot succeed.
    #[error(
        "the codex daemon control socket path `{path}` is {length} bytes, but a Windows \
         unix-socket address is limited to {limit} bytes including the terminating NUL; set a \
         shorter `CODEX_HOME` so the control socket path fits"
    )]
    ControlSocketPathTooLong {
        path: String,
        length: usize,
        limit: usize,
    },
}
