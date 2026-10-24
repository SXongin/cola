//! The server lifecycle seam (spec #860, ticket #875).
//!
//! Discovering a server, attaching to it, checking it is healthy, optionally
//! starting one, yielding an Owned Server to a Coexistent one and reaping it
//! are all **client** concerns, not bridge concerns. OpenCode has a Shared
//! Store, a Coexistent Server and an Owned Server to yield; Codex's daemon is
//! never owned or reaped (ADR-0090). Putting that policy in the bridge would
//! force a `kind` branch into the poll loop — the double rewrite ADR-0090
//! rejects.
//!
//! So the active client implements this narrow trait, and the bridge has one
//! call ([`ServerLifecycle::ensure_attached`]) plus
//! [`AttachOutcome`] as its failure signal. The vocabulary here is neutral:
//! no OpenCode process class, generation strategy or discovery type crosses the
//! seam. The adapter keeps its richer internal types; only the four shapes a
//! caller needs are shared:
//!
//! - [`EnsureAttached`] — what the caller knows (is a message waiting, is a
//!   turn streaming);
//! - [`AttachOutcome`] — attached / serverless / a server it could not
//!   classify;
//! - [`AttachmentStatus`] — the read `cola status` and `cola doctor` report;
//! - [`RestartOutcome`] — `/restart-opencode`'s neutral result.
//!
//! [`ServerLifecycle::ensure_attached`] is deliberately **required**: a new
//! backend cannot silently inherit a healthy-looking "attached" answer while
//! its runtime is actually unreachable. A client with no lifecycle of its own
//! (a peer attachment the user manages, or a test double) implements the
//! explicit "already attached, nothing to do" body instead.

use std::time::Duration;

use async_trait::async_trait;

use crate::error::Result;

use super::Backend;

/// One "ensure attached" request from the bridge.
///
/// The bridge only knows these facts; the client owns the policy.
pub struct EnsureAttached<'a> {
    /// A start is allowed (the demand path, or an `eager` boot). The client
    /// still enforces its own start policy.
    pub allow_spawn: bool,
    /// The poll path may start one only when a turn is in flight, so a
    /// mid-stream generation's next poll finds a live server instead of 502ing.
    pub heal_when_busy: bool,
    /// The busy probe. The client calls it **after it has acquired its
    /// lifecycle lock**, at the migration/yield decision, so the answer
    /// describes the serialized pass rather than the moment the bridge built
    /// the request. A contended read is treated as busy by the bridge (the
    /// conservative answer: never migrate when the state cannot be read).
    pub busy: &'a (dyn Fn() -> bool + Sync),
    /// The process-start pass. A start that cannot resolve a generation is
    /// **not** fatal at boot when a default-store server is alive (the next
    /// reconnect pass attaches once it classifies), and boot does not wait on
    /// the extra request-readiness probe the demand path uses — the start
    /// already resolved the generation. The poll and demand paths leave this
    /// false.
    pub boot: bool,
}

/// The result of one ensure pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachOutcome {
    /// A server is attached (or the client has nothing to manage).
    Attached,
    /// No server is attached. For a client that may spawn, none could be
    /// started under the current policy; for one that may not, this is the
    /// steady state.
    Serverless,
    /// A server was found but could not be classified. The evidence is for the
    /// caller to report; the client is serverless and retries on a later pass.
    Inconclusive { url: String, evidence: String },
}

/// One eligible server that is not the canonical attachment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OtherServer {
    pub url: String,
    pub pid: i32,
}

/// What an attachment looks like to a read-only introspection command
/// (`cola status`, `cola doctor`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentStatus {
    pub url: String,
    pub pid: i32,
    /// The client's own protocol label for display — for an OpenCode
    /// attachment the resolved generation (`v1`/`v2`), for another client its
    /// equivalent opaque label. `None` when the client could not classify the
    /// attachment; never guessed. The seam does not interpret the value.
    pub generation: Option<String>,
    /// The probe's evidence, the detail line when the generation is unknown.
    pub evidence: String,
    /// Other eligible servers the client is not attached to.
    pub other_servers: Vec<OtherServer>,
}

/// One capability the attached runtime advertised, rendered neutrally for
/// `cola status`/`cola doctor` (ADR-0090 decision 3, ticket #883).
///
/// A capability is a **claim the connection can be held to**, not a feature
/// this build merely hopes for: the Codex adapter lists what its verified
/// `initialize` surface covers, and its counts. The seam never interprets the
/// pair — a caller prints it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capability {
    pub name: String,
    pub value: String,
}

/// What is proven about one local process using this client's runtime
/// (ADR-0090 decision 3's diagnosis).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserClientState {
    /// The process names the discovered runtime as its endpoint, so it shares
    /// that runtime's sessions (the supported configuration).
    Attached,
    /// The process runs its own embedded runtime and shares nothing.
    Embedded,
    /// The process may share the runtime, but its argv cannot prove it.
    Unverified,
}

/// One local client process the active adapter observed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserClient {
    pub pid: i32,
    pub binary: String,
    pub state: UserClientState,
    /// Why the state was concluded, for the report and the log.
    pub detail: String,
}

/// The runtime's state as the active client reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientState {
    /// Attached and answering.
    Attached,
    /// A runtime was found but this process could not attach to it.
    Unattached,
    /// No runtime was found.
    Absent,
}

/// The read-only diagnosis `cola status` and `cola doctor` report for the
/// **active** client (ADR-0090 decision 3, ticket #883).
///
/// Deliberately neutral: the id and kind come from the config, and everything
/// an adapter knows about its own runtime — endpoint, capabilities, the local
/// clients it can prove something about — arrives here. The CLI never spells a
/// runtime-specific type, so a second adapter reports through the same shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientReport {
    /// The endpoint this client targets, even when no runtime is serving it
    /// (a unix socket path or a `ws://` URL).
    pub endpoint: String,
    pub state: ClientState,
    /// The evidence for `state` — the daemon's own report, or why it is
    /// absent.
    pub detail: String,
    /// The binary the runtime executes, when it can be learned (Codex's
    /// managed daemon binary is not the CLI on `PATH`).
    pub runtime_binary: Option<String>,
    /// The connected server's version, platform and verified surface.
    pub version: Option<String>,
    pub platform: Option<String>,
    pub surface_source: Option<String>,
    /// What the connected server's verified surface covers.
    pub capabilities: Vec<Capability>,
    /// Local processes using this client, in pid order.
    pub user_clients: Vec<UserClient>,
    /// The client binary this install would run, resolved where the adapter
    /// would spawn it.
    pub client_binary: Option<String>,
}

/// The `cola doctor --check-turn` outcome (ADR-0090's amendment: a turn with no
/// usable credential is accepted and then fails as a retryable `error`, so a
/// login-shaped check would pass while every turn died).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnProbe {
    /// The configured provider completed a minimal turn.
    Passed { detail: String },
    /// The turn failed — a retryable or terminal error, a refusal, a closed
    /// connection, or the budget expiry.
    Failed { detail: String },
    /// The probe could not run (no runtime attached).
    NotAttempted { detail: String },
}

/// `/restart-opencode`'s neutral outcome (ADR-0085: only a server cola started
/// may be restarted).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartOutcome {
    /// An Owned server was restarted.
    Restarted,
    /// The managed Shared Service was restarted.
    RestartedService,
    /// Another tool's server — never touched.
    NotOwned,
    /// No server is running.
    NoServer,
}

/// The active client's discover / attach / health / optionally-start surface.
///
/// [`Self::ensure_attached`] is required: a client must state its attachment
/// explicitly, so a disconnected runtime can never look healthy by inheriting
/// a default. The read-only methods keep defaults stating **this client has
/// nothing to report** — a peer attachment whose daemon the user owns, or a
/// test double.
#[async_trait]
pub trait ServerLifecycle: Send + Sync {
    /// Ensure the client is attached to its runtime, honoring the request's
    /// spawn allowance and busy probe. This is the poll loop's one entry
    /// point; it never returns "which server class"; it returns whether the
    /// client is attached and, when resolution failed, why.
    ///
    /// A client with no lifecycle of its own answers
    /// `Ok(AttachOutcome::Attached)` explicitly.
    async fn ensure_attached(&self, request: EnsureAttached<'_>) -> Result<AttachOutcome>;

    /// The attachment for read-only introspection. `None` when there is
    /// nothing to report (a client whose runtime the user manages elsewhere).
    async fn attachment_status(&self) -> Option<AttachmentStatus> {
        None
    }

    /// The client-scoped diagnosis `cola status`/`cola doctor` report
    /// (ADR-0090 decision 3): which runtime was found, whether the local
    /// client(s) share it, and what the connection advertised. `None` when the
    /// client has no runtime to diagnose.
    ///
    /// Read-only: a client may dial/attach to learn the capabilities, but it
    /// must not start or write to the runtime. The one write a diagnostic may
    /// need is [`Self::probe_turn`], which the CLI gates explicitly.
    async fn client_report(&self) -> Option<ClientReport> {
        None
    }

    /// Establish that the configured provider can actually complete a turn
    /// (ADR-0090's amendment, ticket #883). This is a **real turn** on a
    /// scratch thread — a side effect on a runtime cola does not own — so the
    /// CLI only calls it for `cola doctor --check-turn`, never for a plain
    /// `status` or `doctor`.
    async fn probe_turn(&self) -> TurnProbe {
        TurnProbe::NotAttempted {
            detail: "this client has no turn probe".to_string(),
        }
    }

    /// Restart the runtime **only when this client owns it**. Default: no
    /// server to restart (only an Owned Server may ever be touched).
    async fn restart_server(&self) -> Result<RestartOutcome> {
        Ok(RestartOutcome::NoServer)
    }
}

/// Wait until a freshly spawned server actually serves requests, within
/// `timeout`.
///
/// A server may accept TCP before its HTTP dispatch is fully wired up;
/// requests landing in that startup window are swallowed silently and hang
/// forever — the Lazy Start flow's first request always landed there, hanging
/// the turn silently (the Lazy Start silent-hang incident). Each probe is
/// short-bounded so a wedged attempt is abandoned and retried on a fresh
/// connection. An **HTTP response** — success or error — counts as ready: the
/// failure mode being dodged is a swallowed request (no response at all), and a
/// responding server is past the window even when it answers with an error. A
/// **transport** failure (connect/send/timeout) is not a response and keeps
/// retrying: a connection reset between the generation probe and this probe
/// must not be read as readiness.
pub(crate) async fn wait_for_server_ready<T>(
    backend: &T,
    timeout: Duration,
) -> std::result::Result<(), String>
where
    T: Backend + ?Sized,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        match tokio::time::timeout(Duration::from_millis(1500), backend.list_sessions()).await {
            Ok(Ok(_)) => return Ok(()),
            Ok(Err(error)) if answered(&error) => return Ok(()),
            Ok(Err(_)) | Err(_) => {
                if tokio::time::Instant::now() >= deadline {
                    // The adapter owns the runtime's error vocabulary; this
                    // helper only reports the neutral evidence.
                    return Err("accepted connections but never served a request".into());
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }
}

/// Whether an error proves the server answered, as opposed to a transport
/// failure before any response.
///
/// `BridgeError::Http` carries the reqwest error, so a status / decode / body
/// failure is a response; connect, keep-alive and send failures are not. Every
/// other variant comes from code that ran after a response was read.
fn answered(error: &crate::error::BridgeError) -> bool {
    match error {
        crate::error::BridgeError::Http(error) => {
            !(error.is_connect() || error.is_timeout() || error.is_request())
        }
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_connection_failure_is_not_readiness() {
        // `no_proxy`: an environment proxy would answer for the refused port
        // and turn this into a response, hiding the case under test.
        let error = reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get("http://127.0.0.1:1/")
            .send()
            .await
            .expect_err("port 1 must refuse the connection");
        assert!(
            !answered(&crate::error::BridgeError::Http(error)),
            "a connect/send failure is not an HTTP response"
        );
    }

    #[test]
    fn an_application_error_after_a_response_is_readiness() {
        assert!(answered(&crate::error::BridgeError::OpenCode(
            "request failed: 401 Unauthorized".into()
        )));
    }
}
