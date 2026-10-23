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
/// connection. ANY HTTP response — success or error — counts as ready: the
/// failure mode being dodged is a swallowed request (no response at all), and a
/// responding server is past the window even when it answers with an error.
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
            Ok(Ok(_)) | Ok(Err(_)) => return Ok(()),
            Err(_) => {
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
