//! The Codex daemon's lifecycle: discover, attach, and — only when the policy
//! allows — start it, never owning it (spec #860, ticket #877; ADR-0090
//! decision 3).
//!
//! This is the Codex implementation of [`crate::backend::ServerLifecycle`], the
//! slice the bridge's poll loop calls: one [`ServerLifecycle::ensure_attached`]
//! entry point returning attached / serverless / a server it could not classify.
//! Everything runtime-specific stays here:
//!
//! - discovery of the control socket under the shared `CODEX_HOME` (its own
//!   module, [`super::discovery`]);
//! - the [`crate::config::ServerStartPolicy`] decision: `never` never starts,
//!   `auto`/`eager` may start through Codex's own `daemon start`;
//! - **no ownership at all** — no pid record is written, no yield and no signal
//!   is ever sent, including on shutdown. cola's daemon is another tool's
//!   process, exactly like a Coexistent Server.
//!
//! "Attached" is a claim about an **answering** daemon, not about a socket path:
//! the steady-state pass proves the cached connection with one read-only call,
//! so a crashed daemon that leaves its Unix socket behind reads unattached, and
//! the socket identity is captured around the handshake so a replacement cannot
//! bind two servers to one attachment.
//!
//! A local client on an *embedded* server is diagnosed rather than mirrored
//! (ADR-0090 decision 3): the pass logs it once and [`CodexLifecycle::diagnosis`]
//! reports it for `cola doctor`, while the attach outcome stays `Serverless` —
//! there is no daemon to attach to, and cola will not pretend otherwise. A TUI
//! whose sharing the process table cannot settle is reported as
//! **unverified**, never assumed.
//!
//! `cola doctor`'s provider-turn probe runs by default: ADR-0090 decision 3
//! makes "a configured provider that can complete a turn" the support
//! criterion, and the user invoked the diagnostic explicitly, so its write is
//! within that contract. The probe reserves an exclusive scratch directory,
//! starts a named thread and a minimal turn there, then interrupts the turn
//! while it is live and deletes the thread. `--no-turn-check` is the explicit
//! opt-out, and the doctor check names the write and possible provider spend.
//!
//! The `Backend` implementation that serves Sessions is [`super::backend`]
//! (ticket #879); this type is the lifecycle half it builds on.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::backend::lifecycle::{
    AttachOutcome, Capability, ClientReport, ClientState, EnsureAttached, RestartOutcome, ServerLifecycle,
    TurnProbe, UserClient, UserClientState,
};
use crate::bridge::tasks::TasksHandle;
use crate::config::CodexConfig;
use crate::error::BridgeError;

use super::client::{CodexClient, CodexOptions};
use super::discovery::{
    self, Daemon, DaemonState, DiscoveredDaemon, Discovery, Environment, LocalClients, SocketIdentity,
};
use super::error::{CodexError, Result};
use super::schema::{AutoProbe, SurfaceProbe};
use super::transport::{Endpoint, Inbound, RpcConnection};

/// How the lifecycle builds the schema probe for a discovered daemon.
///
/// Production probes the binary the daemon actually runs; a test substitutes
/// the pinned probe so no real Codex binary is needed.
pub(crate) trait ProbeSource: Send + Sync {
    fn probe(&self, daemon: &Daemon) -> Box<dyn SurfaceProbe>;
}

/// The production source: the managed daemon binary, never the `codex` on
/// `PATH` (ticket #876's finding — the two are different builds).
pub(crate) struct ManagedProbe;

impl ProbeSource for ManagedProbe {
    fn probe(&self, daemon: &Daemon) -> Box<dyn SurfaceProbe> {
        Box::new(AutoProbe::new(daemon.managed_binary.clone()))
    }
}

/// One live attachment and the socket identity it was dialled through.
struct Attachment {
    client: CodexClient,
    identity: SocketIdentity,
}

/// How long the cached connection gets to answer the steady-state liveness
/// probe.
///
/// Deliberately **smaller** than the 10 s handshake budget: the probe is served
/// from the daemon's in-memory thread table over an already-warm connection, so
/// no model, provider or disk work can delay it — a busy daemon (a streaming
/// turn, a compaction) still answers instantly, and a miss therefore means the
/// transport is gone or wedged. Even a false negative is cheap: the attachment
/// is dropped for this pass and the next pass re-attaches; nothing is killed.
/// What the bound buys is that a wedged connection cannot stall the lifecycle
/// locks for the handshake's full budget on every poll tick.
const LIVENESS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// The one request the liveness probe sends. It is in the required contract
/// (ticket #876 checks for it at construction), is server-global and read-only,
/// and answers from the daemon's own state — so `Ok` proves the connection is
/// still served by a live daemon.
const LIVENESS_METHOD: &str = "thread/loaded/list";

/// How many times one attach pass re-dials when the control socket is replaced
/// under it. Two is enough for the deterministic race; a third race in a row is
/// reported unattached and retried on the next pass.
const ATTACH_ATTEMPTS: usize = 2;

/// The budget for `cola doctor`'s real turn probe. A minimal turn still
/// crosses the configured provider, so this is generous; the point of the
/// bound is that an unreachable or wedged provider fails with a clear timeout
/// instead of hanging the command.
#[cfg(not(test))]
const TURN_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
#[cfg(test)]
const TURN_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// A bound on each probe bookkeeping call (`thread/name/set`, `thread/delete`):
/// a wedged connection must not extend the diagnosis past its own turn budget.
const TURN_PROBE_CLEANUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// The probe thread's name. The thread is a real write on a daemon cola does
/// not own, so it is named as cola's own diagnostic and deleted afterwards.
const TURN_PROBE_THREAD_NAME: &str = "cola doctor probe";

/// The smallest prompt that still requires the provider to answer.
const TURN_PROBE_PROMPT: &str =
    "Reply with OK. This is a cola doctor probe; the thread is deleted afterwards.";

/// How one dial ended, for the pass's next decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Attempt {
    Attached,
    /// The daemon was there a moment ago but does not accept now — a stale
    /// socket or a daemon that just died. A start policy may apply.
    Unreachable,
    /// The control path was re-pointed mid-handshake, twice. There *is* a
    /// daemon; it just cannot be attached under this race. Do not start one.
    Raced,
}

/// Whether the cached connection still answers. A dead daemon behind a stale
/// socket path fails here (its connection is closed / times out), which is what
/// separates "same inode" from "same live daemon".
async fn answers(attachment: &mut Attachment) -> bool {
    match tokio::time::timeout(
        LIVENESS_TIMEOUT,
        attachment
            .client
            .connection()
            .request(LIVENESS_METHOD, serde_json::json!({})),
    )
    .await
    {
        Ok(Ok(_)) => true,
        Ok(Err(error)) => {
            tracing::warn!("the cached codex attachment no longer answers `{LIVENESS_METHOD}`: {error}");
            false
        }
        Err(_) => {
            tracing::warn!(
                "the cached codex attachment did not answer `{LIVENESS_METHOD}` within \
                 {LIVENESS_TIMEOUT:?}"
            );
            false
        }
    }
}

/// The active Codex Agent Client's lifecycle.
///
/// Holds the live connection between passes so the poll loop does not redial a
/// daemon that is still there; a replaced socket is re-attached, and a vanished
/// one is reported unattached on the next pass.
pub(crate) struct CodexLifecycle {
    settings: CodexConfig,
    environment: Environment,
    probes: Box<dyn ProbeSource>,
    /// The Background Task State handle the attached clients share with the
    /// bridge (ADR-0079); one per active client, not per connection.
    pub(crate) tasks: TasksHandle,
    /// The turns cola started (ADR-0090): shared by every connection this
    /// lifecycle attaches, and durable when the daemon wires the file.
    authored: Arc<super::authorship::Authorship>,
    /// The current connection. A `tokio` mutex because the attach pass holds it
    /// across the handshake's await.
    attached: tokio::sync::Mutex<Option<Attachment>>,
    /// Serialises ensure passes. The busy probe is read only after this lock is
    /// acquired, so the answer describes this pass rather than the moment the
    /// caller built its request.
    pass: tokio::sync::Mutex<()>,
    /// The embedded client last reported at WARN, so a poll loop does not log
    /// the same diagnosis every tick.
    reported_embedded: AtomicI32,
    /// Test-only: a one-shot hook run after the connection attempt returns and
    /// before the identity is re-read, so a test can re-point the control socket
    /// inside that window (a successful dial and a failed one alike). The field
    /// exists only in a test build.
    #[cfg(test)]
    dial_hook: std::sync::Mutex<Option<std::sync::Arc<dyn Fn() + Send + Sync>>>,
}

impl CodexLifecycle {
    pub(crate) fn new(settings: CodexConfig, environment: Environment, probes: Box<dyn ProbeSource>) -> Self {
        Self {
            settings,
            environment,
            probes,
            tasks: TasksHandle::new(),
            authored: super::authorship::Authorship::in_memory(),
            attached: tokio::sync::Mutex::new(None),
            pass: tokio::sync::Mutex::new(()),
            reported_embedded: AtomicI32::new(0),
            #[cfg(test)]
            dial_hook: std::sync::Mutex::new(None),
        }
    }

    /// Carry the durable record of turns cola started (ADR-0090). The daemon
    /// opens it once at startup so a restart still knows which turns are its
    /// own; a lifecycle left on the in-memory default is correct only for its
    /// own lifetime.
    pub(crate) fn with_authorship(mut self, authored: Arc<super::authorship::Authorship>) -> Self {
        self.authored = authored;
        self
    }

    /// The attached client the `Backend` surface reads through, if the last
    /// attach pass left one installed.
    pub(crate) async fn attached_client(&self) -> Option<CodexClient> {
        self.attached
            .lock()
            .await
            .as_ref()
            .map(|attachment| attachment.client.clone())
    }

    /// The control socket the lifecycle targets, rendered as the neutral base
    /// URL. Stable while serverless too: a replacement daemon keeps the path.
    pub(crate) fn environment_socket(&self) -> std::path::PathBuf {
        self.environment.socket()
    }

    /// Attach for a **diagnostic**: read-only, never a start, never a mid-turn
    /// heal. `cola status`/`doctor` and the turn probe all share this one
    /// setting set so the safety choices cannot drift apart.
    async fn attach_for_diagnostics(&self) -> crate::error::Result<AttachOutcome> {
        let never_busy = || false;
        self.ensure_attached(EnsureAttached {
            allow_spawn: false,
            heal_when_busy: false,
            busy: &never_busy,
            boot: true,
        })
        .await
    }

    /// Test-only: install the one-shot hook that fires after a connection
    /// attempt returns and before its identity is re-read.
    #[cfg(test)]
    pub(crate) fn set_dial_hook(&self, hook: std::sync::Arc<dyn Fn() + Send + Sync>) {
        *self.dial_hook.lock().expect("dial hook") = Some(hook);
    }

    /// What discovery last observed — the read `cola doctor` and `cola status`
    /// need for ADR-0090 decision 3 (daemon presence, the binary it runs,
    /// clients proven to be on an embedded server, clients proven attached to
    /// the daemon, and clients whose sharing cannot be established from the
    /// process table).
    pub(crate) async fn diagnosis(&self) -> Result<Discovery> {
        discovery::discover(&self.environment).await
    }

    /// One attach to a discovered daemon.
    ///
    /// The socket identity is read **before** the dial and again after the
    /// handshake: a re-pointed symlink in between means the connection may
    /// belong to a daemon the path no longer names, so recording the new
    /// identity would bind two servers into one attachment. Such an attach is
    /// void and retried once; a second race reports unattached for this pass.
    ///
    /// A transport-level failure means the daemon went away between the socket
    /// check and the dial: it is reported as unattached and retried on the next
    /// pass, never reaped. A daemon that answers and is unusable (a contract or
    /// probe failure) stays loud — retrying cannot fix it.
    async fn attach(&self, daemon: &Daemon, observed: SocketIdentity) -> Result<Attempt> {
        // The daemon of the attempt together with the identity discovery read
        // for it, never one from an earlier pass and one from a later read: a
        // retry follows a re-pointed path, and the probe must ask the binary
        // *that* daemon runs (a different build is a different schema).
        let mut current = DiscoveredDaemon {
            daemon: daemon.clone(),
            identity: observed,
        };
        for attempt in 0..ATTACH_ATTEMPTS {
            if attempt > 0 {
                current = match self.current_daemon().await? {
                    Some(daemon) => daemon,
                    // The path is gone between attempts: no daemon, so a start
                    // policy may apply.
                    None => return Ok(Attempt::Unreachable),
                };
            }
            let identity = discovery::socket_identity(&current.daemon.socket)?;
            if identity != Some(current.identity) {
                // The path moved after the pass that paired this daemon with
                // its socket: probing it now would pair one daemon's binary
                // with another daemon's socket. Re-discover the pair; a second
                // move in a row ends the pass as a race.
                tracing::warn!(
                    "the codex control socket moved between discovery and the dial; \
                     re-discovering the daemon it names"
                );
                continue;
            }
            let probe = self.probes.probe(&current.daemon);
            let options = CodexOptions::new(Endpoint::Unix(current.daemon.socket.clone()))
                .with_tasks(self.tasks.clone())
                .with_authorship(self.authored.clone());
            let dialed = CodexClient::connect(&options, probe.as_ref()).await;
            // Test-only: a replacement can land between the connection attempt
            // and the identity re-read right below. The hook opens exactly that
            // window, for both a successful dial and a failed one.
            #[cfg(test)]
            self.fire_dial_hook();
            match dialed {
                Ok(client) => {
                    if discovery::socket_identity(&current.daemon.socket)? != Some(current.identity) {
                        tracing::warn!(
                            "the codex control socket at {} was replaced during the attach; \
                             discarding a connection of unknown identity",
                            current.daemon.socket.display()
                        );
                        // Dropping `client` closes the connection to the daemon
                        // the path no longer names. Nothing is sent to it.
                        continue;
                    }
                    tracing::info!(
                        socket = %current.daemon.socket.display(),
                        version = %client.server_info().version,
                        surface = %client.server_info().surface_source,
                        "attached to the codex app-server daemon"
                    );
                    *self.attached.lock().await = Some(Attachment {
                        client,
                        identity: current.identity,
                    });
                    return Ok(Attempt::Attached);
                }
                Err(error) if daemon_gone(&error) => {
                    // A failed dial can race a replacement too: A refused while
                    // the path moved to a healthy B. Reporting `Unreachable`
                    // would let a start run against B, so re-check the identity
                    // and re-dial the daemon the path now names.
                    if discovery::socket_identity(&current.daemon.socket)? != Some(current.identity) {
                        tracing::warn!(
                            "the codex control socket at {} was replaced while the dial failed; \
                             re-dialling the daemon the path now names",
                            current.daemon.socket.display()
                        );
                        continue;
                    }
                    tracing::warn!(
                        "the codex daemon at {} is gone or unreachable; staying unattached: {}",
                        current.daemon.socket.display(),
                        error
                    );
                    return Ok(Attempt::Unreachable);
                }
                Err(error) => return Err(error),
            }
        }
        Ok(Attempt::Raced)
    }

    /// The daemon the control path names right now and the identity the same
    /// discovery pass read for it — one pair, never mixed.
    async fn current_daemon(&self) -> Result<Option<DiscoveredDaemon>> {
        discovery::discover_daemon(&self.environment).await
    }

    /// Test-only: run the dial hook once, after a connection attempt and before
    /// its identity is re-read. It lets a test re-point the control socket
    /// inside the window the transport-race fix covers.
    #[cfg(test)]
    fn fire_dial_hook(&self) {
        let hook = self.dial_hook.lock().expect("dial hook").take();
        if let Some(hook) = hook {
            hook();
        }
    }

    /// Log the local clients a discovery pass observed: a proven embedded
    /// server once per client at WARN (ADR-0090 decision 3 — the alternative is
    /// a bot that silently mirrors nothing), and an unprovable sharing state at
    /// DEBUG for `cola doctor` to settle, never as a sharing claim.
    fn report_clients(&self, clients: &LocalClients) {
        if let Some(client) = clients.embedded.first()
            && self.reported_embedded.swap(client.pid, Ordering::Relaxed) != client.pid
        {
            tracing::warn!(
                "a local Codex client (pid {}, {}) is running on an embedded app-server; it shares \
                 no thread with the shared daemon, so cola cannot mirror it — start the daemon (and \
                 attach the client to it) for Peer Attachment",
                client.pid,
                client.evidence
            );
        }
        for client in &clients.attached {
            tracing::debug!(
                "a local Codex client (pid {}, {}) is attached to the shared daemon at {}",
                client.pid,
                client.binary,
                client.endpoint
            );
        }
        for client in &clients.unverified {
            tracing::debug!(
                "a local Codex client (pid {}, {}) is not proven attached: {}",
                client.pid,
                client.binary,
                client.detail
            );
        }
        for client in &clients.unreachable {
            tracing::warn!(
                "a local Codex client (pid {}, {}) {}; it shares no thread with the daemon cola \
                 mirrors",
                client.pid,
                client.binary,
                client.detail
            );
        }
    }
}

/// The report before any discovery: the endpoint this client targets and the
/// binary a spawn would use. The rest is filled by [`CodexLifecycle::client_report`].
fn client_report_shell(environment: &Environment, endpoint: String) -> ClientReport {
    ClientReport {
        endpoint,
        state: ClientState::Absent,
        detail: String::new(),
        runtime_binary: None,
        version: None,
        platform: None,
        surface_source: None,
        capabilities: Vec::new(),
        user_clients: Vec::new(),
        client_binary: discovery::which_client(&environment.cli).map(|path| path.display().to_string()),
    }
}

/// What discovery proved about each local client, in pid order, for the report.
fn report_user_clients(clients: &LocalClients) -> Vec<UserClient> {
    let mut report = Vec::new();
    for client in &clients.embedded {
        report.push(UserClient {
            pid: client.pid,
            binary: client.binary.clone(),
            state: UserClientState::Embedded,
            detail: client.evidence.to_string(),
        });
    }
    for client in &clients.attached {
        report.push(UserClient {
            pid: client.pid,
            binary: client.binary.clone(),
            state: UserClientState::Attached,
            detail: client.detail.clone(),
        });
    }
    for client in &clients.unverified {
        report.push(UserClient {
            pid: client.pid,
            binary: client.binary.clone(),
            state: UserClientState::Unverified,
            detail: client.detail.clone(),
        });
    }
    for client in &clients.unreachable {
        report.push(UserClient {
            pid: client.pid,
            binary: client.binary.clone(),
            state: UserClientState::Unreachable,
            detail: client.detail.clone(),
        });
    }
    report.sort_by_key(|client| client.pid);
    report
}

/// The capabilities a verified app-server connection advertises. The method
/// names are the pinned contract's, which construction refuses to attach
/// without, so every one of these is a claim the connection was held to — not
/// a hope. Background Tasks are the one surface Codex does not publish.
fn codex_capabilities(info: &super::client::ServerInfo) -> Vec<Capability> {
    let (client_requests, notifications, server_requests) = info.surface_counts;
    let mut capabilities = vec![
        Capability {
            name: "peer_attachment".to_string(),
            value: "yes".to_string(),
        },
        Capability {
            name: "background_tasks".to_string(),
            value: "unsupported".to_string(),
        },
        Capability {
            name: "turn_control".to_string(),
            value: "turn/start, turn/steer, turn/interrupt".to_string(),
        },
        Capability {
            name: "permissions".to_string(),
            value: "yes".to_string(),
        },
        Capability {
            name: "questions".to_string(),
            value: "yes".to_string(),
        },
        Capability {
            name: "compaction".to_string(),
            value: "thread/compact/start".to_string(),
        },
        Capability {
            name: "models".to_string(),
            value: "model/list".to_string(),
        },
        Capability {
            name: "skills".to_string(),
            value: "skills/list".to_string(),
        },
        Capability {
            name: "surface".to_string(),
            value: info.surface_source.clone(),
        },
    ];
    capabilities.push(Capability {
        name: "client_requests".to_string(),
        value: client_requests.to_string(),
    });
    capabilities.push(Capability {
        name: "notifications".to_string(),
        value: notifications.to_string(),
    });
    capabilities.push(Capability {
        name: "server_requests".to_string(),
        value: server_requests.to_string(),
    });
    capabilities
}

/// Whether an `error` notification names the probe's own turn (some errors are
/// thread-scoped and carry no turn id: those do apply).
fn turn_matches(params: &Value, turn_id: &str) -> bool {
    match params.get("turnId").and_then(Value::as_str) {
        Some(id) => turn_id.is_empty() || id == turn_id,
        None => true,
    }
}

/// The reason a discovery pass found no daemon, as the report's evidence.
fn absent_reason(discovery: &Discovery) -> String {
    match &discovery.daemon {
        DaemonState::Absent { evidence } => evidence.clone(),
        DaemonState::Running(_) => "the daemon is running but was not attached".to_string(),
    }
}

/// How many name suffixes one probe-directory reservation tries before it gives
/// up. A collision is only possible against another process using the same pid
/// and clock value, so a handful is generous; exhausting them is a named
/// failure rather than a silent reuse.
const PROBE_DIR_ATTEMPTS: u32 = 8;

/// Reserve a scratch directory this run **owns**.
///
/// The directory is the only handle on a probe thread whose `thread/start`
/// reply was lost, so cleanup must never treat a pre-existing directory as its
/// own: `create_dir` (not `create_dir_all`) fails when the path exists, and the
/// attempt suffix moves on. Only threads whose cwd is the returned directory
/// may be deleted by recovery.
fn create_probe_directory() -> std::io::Result<std::path::PathBuf> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    create_probe_directory_with(&std::env::temp_dir(), std::process::id(), nanos)
}

/// [`create_probe_directory`] against an explicit root and name seed, so the
/// exclusive reservation is unit-testable.
fn create_probe_directory_with(root: &Path, pid: u32, nanos: u128) -> std::io::Result<std::path::PathBuf> {
    let mut last = None;
    for attempt in 0..PROBE_DIR_ATTEMPTS {
        let path = root.join(format!("cola-doctor-{pid}-{}", nanos + u128::from(attempt)));
        match create_private_dir(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => last = Some(error),
            Err(error) => return Err(error),
        }
    }
    Err(last.unwrap_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "could not reserve a unique cola doctor probe directory",
        )
    }))
}

/// Create one directory, unreadable to other users on Unix. The probe thread
/// runs in it, so its scratch state must not be shared by a second user.
fn create_private_dir(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().mode(0o700).create(path)
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir(path)
    }
}

/// Owns a probe's scratch thread so cancellation cannot leak it.
///
/// The normal path calls [`Self::finish`], which reports a cleanup failure in
/// the probe result. If the future is dropped first — a cancelled doctor, a
/// wrapping timeout — `Drop` spawns the same cleanup on the still-live runtime;
/// the spawned task cannot be awaited, so its failure is logged.
struct ProbeThread {
    connection: RpcConnection,
    directory: std::path::PathBuf,
    thread_id: Option<String>,
    turn_id: Option<String>,
    finished: bool,
}

impl ProbeThread {
    fn new(connection: RpcConnection, directory: std::path::PathBuf) -> Self {
        Self {
            connection,
            directory,
            thread_id: None,
            turn_id: None,
            finished: false,
        }
    }

    /// Record the id as soon as `thread/start` returns, before anything can
    /// fail or be cancelled with the thread already created.
    fn set_thread(&mut self, thread_id: String) {
        self.thread_id = Some(thread_id);
    }

    fn set_turn(&mut self, turn_id: String) {
        self.turn_id = Some(turn_id);
    }

    /// Delete the thread, interrupting the turn only when the probe did not
    /// see it complete, then fall back to a by-directory lookup for an
    /// ambiguous create. Returns the failure when the probe's write is not
    /// gone.
    async fn finish(&mut self, stop_turn: bool) -> Option<String> {
        let turn_id = if stop_turn { self.turn_id.as_deref() } else { None };
        let cleanup = clean_probe_thread(
            &self.connection,
            self.thread_id.as_deref(),
            turn_id,
            &self.directory,
        )
        .await
        .err();
        self.finished = true;
        let _ = std::fs::remove_dir_all(&self.directory);
        cleanup
    }
}

impl Drop for ProbeThread {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let connection = self.connection.clone();
        let directory = self.directory.clone();
        let thread_id = self.thread_id.clone();
        let turn_id = self.turn_id.clone();
        runtime.spawn(async move {
            if let Err(failure) =
                clean_probe_thread(&connection, thread_id.as_deref(), turn_id.as_deref(), &directory).await
            {
                tracing::warn!("the cancelled codex doctor probe could not clean up: {failure}");
            }
            let _ = std::fs::remove_dir_all(&directory);
        });
    }
}

/// Remove the probe's turn and thread. An unknown id is not "nothing to do":
/// an ambiguous `thread/start` may have created the thread, so the distinctive
/// directory is looked up and every match deleted.
async fn clean_probe_thread(
    connection: &RpcConnection,
    thread_id: Option<&str>,
    turn_id: Option<&str>,
    directory: &Path,
) -> std::result::Result<(), String> {
    let mut failures = Vec::new();
    let mut deleted = false;
    if let Some(thread_id) = thread_id {
        if let Some(turn_id) = turn_id {
            match tokio::time::timeout(
                TURN_PROBE_CLEANUP_TIMEOUT,
                connection.request(
                    "turn/interrupt",
                    json!({ "threadId": thread_id, "turnId": turn_id }),
                ),
            )
            .await
            {
                Ok(Ok(_)) => {}
                Ok(Err(error)) => failures.push(format!("turn/interrupt: {error}")),
                Err(_) => failures.push(format!(
                    "turn/interrupt timed out after {TURN_PROBE_CLEANUP_TIMEOUT:?}"
                )),
            }
        }
        match tokio::time::timeout(
            TURN_PROBE_CLEANUP_TIMEOUT,
            connection.request("thread/delete", json!({ "threadId": thread_id })),
        )
        .await
        {
            Ok(Ok(_)) => deleted = true,
            Ok(Err(error)) => failures.push(format!("thread/delete: {error}")),
            Err(_) => failures.push(format!(
                "thread/delete timed out after {TURN_PROBE_CLEANUP_TIMEOUT:?}"
            )),
        }
    }
    // An unknown id (an ambiguous create) or a failed delete still leaves the
    // probe's own thread behind: find it by its unique directory.
    if !deleted {
        match recover_probe_threads(connection, directory).await {
            Ok(0) => {}
            Ok(count) => tracing::info!("deleted {count} codex doctor probe thread(s)"),
            Err(failure) => failures.push(failure),
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("; "))
    }
}

/// Delete every thread whose cwd is the probe's unique directory. This is the
/// only handle on a thread the server created but whose `thread/start` reply
/// was lost.
async fn recover_probe_threads(
    connection: &RpcConnection,
    directory: &Path,
) -> std::result::Result<usize, String> {
    let cwd = directory.to_string_lossy().to_string();
    let result = tokio::time::timeout(
        TURN_PROBE_CLEANUP_TIMEOUT,
        connection.request("thread/list", json!({ "cwd": cwd, "limit": 10 })),
    )
    .await
    .map_err(|_| format!("thread/list timed out after {TURN_PROBE_CLEANUP_TIMEOUT:?}"))?
    .map_err(|error| format!("thread/list: {error}"))?;
    let threads = result
        .get("data")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut deleted = 0;
    let mut failures = Vec::new();
    for thread in threads {
        let Some(id) = thread.get("id").and_then(Value::as_str) else {
            continue;
        };
        if thread.get("cwd").and_then(Value::as_str) != Some(cwd.as_str()) {
            continue;
        }
        match tokio::time::timeout(
            TURN_PROBE_CLEANUP_TIMEOUT,
            connection.request("thread/delete", json!({ "threadId": id })),
        )
        .await
        {
            Ok(Ok(_)) => deleted += 1,
            Ok(Err(error)) => failures.push(format!("thread/delete {id}: {error}")),
            Err(_) => failures.push(format!(
                "thread/delete {id} timed out after {TURN_PROBE_CLEANUP_TIMEOUT:?}"
            )),
        }
    }
    if failures.is_empty() {
        Ok(deleted)
    } else {
        Err(failures.join("; "))
    }
}

/// Append a cleanup failure to a probe detail so it is visible in the result.
fn with_cleanup(detail: String, cleanup: Option<String>) -> String {
    match cleanup {
        Some(cleanup) => format!("{detail}; the probe could not clean up: {cleanup}"),
        None => detail,
    }
}

/// Whether an attach failure proves the daemon is no longer there. The socket
/// existed a moment ago; a refused dial, a closed stream or a wedged handshake
/// is the daemon going away, not a contract this adapter must refuse.
fn daemon_gone(error: &CodexError) -> bool {
    matches!(
        error,
        CodexError::Transport { .. } | CodexError::Timeout(_) | CodexError::NoResponse(_)
    )
}

/// The seam's neutral error; the detail keeps the adapter's own named failure.
fn client_error(error: CodexError) -> BridgeError {
    BridgeError::Client {
        client: "codex",
        detail: error.to_string(),
    }
}

#[async_trait]
impl ServerLifecycle for CodexLifecycle {
    async fn ensure_attached(&self, request: EnsureAttached<'_>) -> crate::error::Result<AttachOutcome> {
        let _pass = self.pass.lock().await;

        // Steady state: the socket we dialled is still there (same inode) **and
        // the cached connection still answers**. Identity alone proves nothing —
        // a crashed daemon can leave its Unix socket path behind — so a failed
        // liveness probe drops the attachment and falls through to a re-attach,
        // which is what turns a stale socket into "unattached". The probe is a
        // read-only call in the pinned contract, never a signal to the daemon.
        let identity = discovery::socket_identity(&self.environment.socket()).map_err(client_error)?;
        if let Some(identity) = identity {
            let mut slot = self.attached.lock().await;
            if slot
                .as_ref()
                .is_some_and(|attachment| attachment.identity == identity)
            {
                let alive = match slot.as_mut() {
                    Some(attachment) => answers(attachment).await,
                    None => false,
                };
                if alive {
                    // The path can move while the old daemon keeps answering:
                    // `Attached` must describe the daemon the control path names
                    // now, so re-read the identity after the probe and fall
                    // through to a re-attach when it changed.
                    let still =
                        discovery::socket_identity(&self.environment.socket()).map_err(client_error)?;
                    if still == Some(identity) {
                        drop(slot);
                        self.report_clients(&discovery::local_clients(Some(
                            self.environment.socket().as_path(),
                        )));
                        return Ok(AttachOutcome::Attached);
                    }
                    tracing::warn!(
                        "the codex control socket at {} was replaced while the previous daemon \
                         still answered; re-attaching to the daemon the path names now",
                        self.environment.socket().display()
                    );
                }
                // The connection is dead, wedged, or no longer the one the path
                // names: forget it. Nothing is sent to the daemon — it is not
                // cola's to stop.
                slot.take();
            }
        }

        let discovered = discovery::discover(&self.environment)
            .await
            .map_err(client_error)?;
        self.report_clients(&discovered.clients);

        // Gone or replaced: drop the stale connection.
        self.attached.lock().await.take();

        // A discovered daemon is dialled; one behind a stale socket fails here
        // as unreachable (`Serverless`), which is exactly the case the start
        // policy below must get a chance at. A daemon that answers and refuses
        // the contract is a different thing and stays an error.
        if let Some(daemon) = discovered.running() {
            match self
                .attach(&daemon.daemon, daemon.identity)
                .await
                .map_err(client_error)?
            {
                Attempt::Attached => return Ok(AttachOutcome::Attached),
                // A daemon exists; it just lost a race with its own control
                // path. Starting another would be wrong, so report unattached
                // and let the next pass dial the daemon the path names.
                Attempt::Raced => return Ok(AttachOutcome::Serverless),
                Attempt::Unreachable => {}
            }
        }

        // No answering daemon. `never` is enforced here, on every path, which is
        // why the policy is checked before the request's allowances: a
        // demand-path or mid-turn-heal start can never override it.
        let want_start = self.settings.start_server.spawns_when_needed()
            && (request.allow_spawn || (request.heal_when_busy && (request.busy)()));
        if !want_start {
            return Ok(AttachOutcome::Serverless);
        }

        // The launcher's result is the daemon **and** the identity the same
        // pass read for it — a cold start's new socket, or the daemon that was
        // already serving when the idempotent command did nothing.
        let started = discovery::start(&self.environment).await.map_err(client_error)?;
        tracing::info!("the codex app-server daemon is serving after its own `app-server daemon start`");
        Ok(
            match self
                .attach(&started.daemon, started.identity)
                .await
                .map_err(client_error)?
            {
                Attempt::Attached => AttachOutcome::Attached,
                Attempt::Unreachable | Attempt::Raced => AttachOutcome::Serverless,
            },
        )
    }

    /// Restarting is never cola's call: the daemon is not owned or reaped
    /// (ADR-0090 decision 3). A live daemon is another tool's server
    /// (`NotOwned`); with none there is nothing to restart (`NoServer`).
    async fn restart_server(&self) -> crate::error::Result<RestartOutcome> {
        let discovered = discovery::discover(&self.environment)
            .await
            .map_err(client_error)?;
        Ok(match discovered.daemon {
            DaemonState::Running(_) => RestartOutcome::NotOwned,
            DaemonState::Absent { .. } => RestartOutcome::NoServer,
        })
    }

    /// ADR-0090 decision 3's read-only diagnosis: the daemon discovery found,
    /// the clients it can prove something about, and the capabilities the
    /// **attached** connection advertised. Attaching here is read-only — the
    /// spawn allowance is always false, so `cola status`/`doctor` never start a
    /// daemon.
    async fn client_report(&self) -> Option<ClientReport> {
        let mut report = client_report_shell(
            &self.environment,
            format!("unix://{}", self.environment.socket().display()),
        );
        if let Err(error) = discovery::socket_identity(&self.environment.socket()) {
            report.state = ClientState::Unattached;
            report.detail = error.to_string();
            return Some(report);
        }
        let mut discovered = match self.diagnosis().await {
            Ok(discovered) => discovered,
            Err(error) => {
                report.state = ClientState::Unattached;
                report.detail = error.to_string();
                return Some(report);
            }
        };
        let daemon = discovered.daemon.daemon().cloned();
        if let Some(daemon) = &daemon {
            // An argv match is not proof; settle it from the clients' open
            // descriptors where the OS exposes them.
            discovery::prove_attachments(&mut discovered.clients, &daemon.socket).await;
        }
        report.detail = match &daemon {
            Some(daemon) => {
                let version = daemon
                    .info
                    .as_ref()
                    .and_then(|info| info.app_server_version.clone())
                    .unwrap_or_else(|| "version unknown".to_string());
                format!("daemon at {} (app-server {version})", daemon.socket.display())
            }
            None => absent_reason(&discovered),
        };
        report.runtime_binary = daemon
            .as_ref()
            .map(|daemon| daemon.managed_binary.display().to_string());
        report.user_clients = report_user_clients(&discovered.clients);
        if daemon.is_none() {
            return Some(report);
        }

        // Attach to read what the connection advertises. The shared diagnostic
        // request never starts the daemon it is describing.
        let outcome = self.attach_for_diagnostics().await;
        match outcome {
            Ok(AttachOutcome::Attached) => match self.attached_client().await {
                Some(client) => {
                    let info = client.server_info();
                    report.state = ClientState::Attached;
                    report.detail = format!(
                        "attached to the app-server daemon (version {}, {})",
                        info.version, info.platform
                    );
                    report.version = Some(info.version.to_string());
                    report.platform = Some(info.platform.clone());
                    report.surface_source = Some(info.surface_source.clone());
                    report.capabilities = codex_capabilities(info);
                }
                None => {
                    report.state = ClientState::Unattached;
                    report.detail = "the daemon answered but no client was installed".to_string();
                }
            },
            Ok(_) => {
                report.state = ClientState::Unattached;
                report.detail = "the daemon is running but cola could not attach to it".to_string();
            }
            Err(error) => {
                report.state = ClientState::Unattached;
                report.detail = error.to_string();
            }
        }
        Some(report)
    }

    /// ADR-0090's amendment: a turn with no usable credential is **accepted**
    /// and then fails as a retryable `error`, so doctor's proof that the
    /// configured provider works is a real turn. This is a write on a daemon
    /// cola does not own, so the CLI runs it by default and offers
    /// `--no-turn-check` as the explicit opt-out.
    ///
    /// The scratch thread is named as cola's diagnostic and cleaned up on
    /// every exit path, including cancellation: a [`ProbeThread`] guard owns
    /// the ids and spawns the same cleanup when the future is dropped. A
    /// retryable error is read from the raw notification stream: the read
    /// model deliberately keeps a retrying turn live (it may recover), so the
    /// probe cannot learn it from the projection.
    async fn probe_turn(&self) -> TurnProbe {
        if let Err(error) = self.attach_for_diagnostics().await {
            return TurnProbe::Failed {
                detail: format!("could not attach to the codex daemon: {error}"),
            };
        }
        let Some(client) = self.attached_client().await else {
            return TurnProbe::NotAttempted {
                detail: "no codex app-server daemon is attached; start the daemon and attach your \
                         client, then retry"
                    .to_string(),
            };
        };
        let directory = match create_probe_directory() {
            Ok(directory) => directory,
            Err(error) => {
                return TurnProbe::Failed {
                    detail: format!("could not reserve a probe directory: {error}"),
                };
            }
        };
        let mut probe = ProbeThread::new(client.connection().clone(), directory.clone());
        let mut inbound = client.connection().inbound();
        let cwd = directory.to_string_lossy().to_string();

        let outcome = tokio::time::timeout(TURN_PROBE_TIMEOUT, async {
            let thread = client
                .threads()
                .start(Some(&cwd))
                .await
                .map_err(|error| format!("could not create a probe thread: {error}"))?;
            let thread_id = thread.id.clone();
            probe.set_thread(thread_id.clone());
            // Name it as cola's own diagnostic; a failure here is cosmetic.
            let _ = tokio::time::timeout(
                TURN_PROBE_CLEANUP_TIMEOUT,
                client.connection().request(
                    "thread/name/set",
                    json!({ "threadId": thread_id, "name": TURN_PROBE_THREAD_NAME }),
                ),
            )
            .await;

            let started = client
                .connection()
                .request(
                    "turn/start",
                    json!({
                        "threadId": thread_id,
                        "input": [{
                            "type": "text",
                            "text": TURN_PROBE_PROMPT,
                            "text_elements": [],
                        }],
                    }),
                )
                .await
                .map_err(|error| format!("the server refused `turn/start`: {error}"))?;
            let turn_id = started
                .pointer("/turn/id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if !turn_id.is_empty() {
                probe.set_turn(turn_id.clone());
            }

            loop {
                match inbound.recv().await {
                    Ok(Inbound::Notification { method, params, .. }) if method == "error" => {
                        if params.get("threadId").and_then(Value::as_str) != Some(thread_id.as_str())
                            || !turn_matches(&params, &turn_id)
                        {
                            continue;
                        }
                        let message = params
                            .pointer("/error/message")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown error");
                        if params.get("willRetry").and_then(Value::as_bool).unwrap_or(false) {
                            return Err(format!(
                                "the configured provider cannot complete a turn (the server will \
                                 retry): {message}"
                            ));
                        }
                        return Err(format!("the turn failed: {message}"));
                    }
                    Ok(Inbound::Notification { method, params, .. }) if method == "turn/completed" => {
                        if params.get("threadId").and_then(Value::as_str) != Some(thread_id.as_str()) {
                            continue;
                        }
                        let Some(turn) = params.get("turn") else {
                            continue;
                        };
                        let id = turn.get("id").and_then(Value::as_str).unwrap_or_default();
                        if !id.is_empty() && !turn_id.is_empty() && id != turn_id {
                            continue;
                        }
                        let status = turn.get("status").and_then(Value::as_str).unwrap_or("unknown");
                        if status == "completed" {
                            return Ok(format!(
                                "the configured provider completed a turn (thread {thread_id})"
                            ));
                        }
                        let error = turn
                            .get("error")
                            .and_then(|error| error.get("message"))
                            .and_then(Value::as_str);
                        return Err(match error {
                            Some(message) => format!("the turn ended as {status}: {message}"),
                            None => format!("the turn ended as {status}"),
                        });
                    }
                    Ok(Inbound::Closed) | Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        return Err("the connection closed before the turn completed".to_string());
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Ok(_) => continue,
                }
            }
        })
        .await;

        let stop_turn = !matches!(&outcome, Ok(Ok(_)));
        let cleanup = probe.finish(stop_turn).await;
        match outcome {
            Ok(Ok(detail)) => TurnProbe::Passed { detail, cleanup },
            Ok(Err(detail)) => TurnProbe::Failed {
                detail: with_cleanup(detail, cleanup),
            },
            Err(_) => TurnProbe::Failed {
                detail: with_cleanup(
                    format!("the turn did not complete within {TURN_PROBE_TIMEOUT:?}"),
                    cleanup,
                ),
            },
        }
    }
}

// The lifecycle's tests drive a real unix socket (the daemon transport is
// unix-only on this side); the type itself still compiles on every platform.
#[cfg(all(test, unix))]
mod tests {
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use super::*;
    use crate::codex::schema::PinnedProbe;
    use crate::codex::test_server::{FakeCodex, FakeServer, ScriptedThread};

    /// Record which binary each attach asks the probe for, and answer with the
    /// pinned surface (the fake daemon reports the pinned version).
    struct RecordingProbe {
        seen: Arc<Mutex<Vec<PathBuf>>>,
    }

    impl ProbeSource for RecordingProbe {
        fn probe(&self, daemon: &Daemon) -> Box<dyn SurfaceProbe> {
            self.seen
                .lock()
                .expect("probe log")
                .push(daemon.managed_binary.clone());
            Box::new(PinnedProbe)
        }
    }

    /// The pinned probe, plus one side effect on the first `surface()` call:
    /// re-point the control symlink **during** the handshake, after it has
    /// reached the old daemon. That is the identity race `attach` must refuse to
    /// record. (`surface()` is called after `initialize`, inside
    /// `CodexClient::connect`, so this lands mid-attach.)
    struct RepointingProbe {
        once: Arc<std::sync::Once>,
        control: PathBuf,
        target: PathBuf,
        seen: Arc<Mutex<Vec<PathBuf>>>,
    }

    impl ProbeSource for RepointingProbe {
        fn probe(&self, daemon: &Daemon) -> Box<dyn SurfaceProbe> {
            self.seen
                .lock()
                .expect("probe log")
                .push(daemon.managed_binary.clone());
            Box::new(RepointingSurface {
                once: Arc::clone(&self.once),
                control: self.control.clone(),
                target: self.target.clone(),
            })
        }
    }

    struct RepointingSurface {
        once: Arc<std::sync::Once>,
        control: PathBuf,
        target: PathBuf,
    }

    #[async_trait]
    impl SurfaceProbe for RepointingSurface {
        async fn surface(
            &self,
            server: &super::super::client::ServerVersion,
        ) -> Result<super::super::schema::Surface> {
            self.once.call_once(|| {
                std::fs::remove_file(&self.control).expect("remove the old link");
                std::os::unix::fs::symlink(&self.target, &self.control).expect("re-point the link");
            });
            PinnedProbe.surface(server).await
        }

        fn source(&self) -> String {
            PinnedProbe.source()
        }
    }

    struct Fixture {
        root: tempfile::TempDir,
        environment: Environment,
        cli: FakeCodex,
        daemon_socket: PathBuf,
        daemon_pid: PathBuf,
        probe_binaries: Arc<Mutex<Vec<PathBuf>>>,
    }

    impl Fixture {
        /// A relocated CODEX_HOME, a fake management CLI, and a real unix
        /// socket for the fake daemon. Nothing is started: the fake CLI links
        /// the control socket only when `daemon start` runs.
        fn new() -> Self {
            let root = tempfile::tempdir().expect("tempdir");
            let home = root.path().join("codex-home");
            let daemon_socket = root.path().join("daemon-0").join("app-server-control");
            std::fs::create_dir_all(daemon_socket.parent().expect("daemon dir")).expect("daemon dir");
            let daemon_pid = root.path().join("fake-daemon.pid");
            let control = home.join(discovery::CONTROL_DIR).join(discovery::CONTROL_SOCKET);
            let cli = FakeCodex::write(
                root.path(),
                &control,
                Some(&daemon_socket),
                Some(&daemon_pid),
                &root.path().join("managed-codex"),
            );
            let environment = Environment {
                home,
                cli: cli.binary.clone(),
                version_timeout: Duration::from_secs(5),
                start_timeout: Duration::from_secs(5),
                socket_ready_timeout: Duration::from_millis(500),
            };
            Self {
                root,
                environment,
                cli,
                daemon_socket,
                daemon_pid,
                probe_binaries: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn lifecycle(&self, start_server: crate::config::ServerStartPolicy) -> CodexLifecycle {
            CodexLifecycle::new(
                CodexConfig { start_server },
                self.environment.clone(),
                Box::new(RecordingProbe {
                    seen: Arc::clone(&self.probe_binaries),
                }),
            )
        }

        /// A state layout under the same temp root, used to prove cola writes
        /// no pid record of its own.
        fn layout(&self) -> crate::state::StateLayout {
            crate::state::StateLayout::for_home(self.root.path())
        }

        /// Link the control socket at the daemon the fixture serves, exactly as
        /// `codex app-server daemon start` does.
        fn link_daemon(&self) {
            let control = self.environment.socket();
            std::fs::create_dir_all(control.parent().expect("control dir")).expect("control dir");
            std::os::unix::fs::symlink(&self.daemon_socket, &control).expect("symlink the daemon");
        }

        /// Spawn a fake local `codex` TUI: an `sh` symlink named `codex` keeps
        /// the argv the process scan reads (no interpreter re-exec).
        fn spawn_tui(&self, extra: &[&str]) -> std::process::Child {
            let tui_dir = self.root.path().join("tui");
            std::fs::create_dir_all(&tui_dir).expect("tui dir");
            let binary = tui_dir.join("codex");
            if !binary.exists() {
                std::os::unix::fs::symlink("/bin/sh", &binary).expect("symlink");
            }
            let mut command = std::process::Command::new(&binary);
            command.args(["-c", "sleep 30; :"]);
            command.args(extra);
            command.spawn().expect("spawn the fake TUI")
        }

        /// Spawn a fake `codex` TUI that **actually connects** to `socket` and
        /// sleeps, so the descriptor proof can be exercised end to end. `None`
        /// when `python3` is unavailable (the test then skips).
        fn spawn_connected_tui(&self, socket: &std::path::Path, remote: &str) -> Option<std::process::Child> {
            let python = ["python3", "/usr/bin/python3", "/usr/sbin/python3"]
                .into_iter()
                .find(|program| {
                    std::process::Command::new(program)
                        .arg("--version")
                        .output()
                        .is_ok()
                })?;
            let tui_dir = self.root.path().join("tui-connected");
            std::fs::create_dir_all(&tui_dir).expect("connected tui dir");
            let binary = tui_dir.join("codex");
            if !binary.exists() {
                std::os::unix::fs::symlink(python, &binary).expect("symlink python");
            }
            let script = "import socket,sys,time; s=socket.socket(socket.AF_UNIX); \
                          s.connect(sys.argv[1]); time.sleep(30)";
            std::process::Command::new(&binary)
                .args(["-c", script, &socket.display().to_string(), "--remote", remote])
                .spawn()
                .ok()
        }

        /// The fake daemons' own pid records, once their launcher has run.
        fn daemon_pids(&self) -> Vec<i32> {
            std::fs::read_to_string(&self.daemon_pid)
                .unwrap_or_default()
                .lines()
                .filter_map(|line| line.trim().parse().ok())
                .collect()
        }

        /// Stop the fake daemon processes the test spawned; cola never does.
        fn reap(&self) {
            for pid in self.daemon_pids() {
                let _ = crate::process::force_kill(pid);
            }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            self.reap();
        }
    }

    fn request(busy: &(dyn Fn() -> bool + Sync)) -> EnsureAttached<'_> {
        EnsureAttached {
            allow_spawn: true,
            heal_when_busy: false,
            busy,
            boot: false,
        }
    }

    /// `start_server = "never"` with no daemon: unattached, and nothing is even
    /// asked to start. The fake CLI is never invoked, so there is no start to
    /// mistake.
    #[tokio::test]
    async fn attach_only_reports_unattached_and_starts_nothing() {
        let fixture = Fixture::new();
        let lifecycle = fixture.lifecycle(crate::config::ServerStartPolicy::Never);
        let never_busy = || true;

        assert_eq!(
            lifecycle
                .ensure_attached(EnsureAttached {
                    heal_when_busy: true,
                    ..request(&never_busy)
                })
                .await
                .expect("attach-only is not a failure"),
            AttachOutcome::Serverless
        );
        assert_eq!(fixture.cli.starts(), 0, "{:?}", fixture.cli.invocations());
        assert!(
            fixture.cli.invocations().is_empty(),
            "discovery of an absent daemon runs no command: {:?}",
            fixture.cli.invocations()
        );
        assert!(!fixture.environment.socket().exists());
    }

    /// The acceptance path: `auto`, no daemon, one daemon started through the
    /// CLI, attached — and then left alone. No pid record, no signal, and the
    /// process outlives the client.
    #[tokio::test]
    async fn auto_starts_one_daemon_then_attaches_and_never_owns_it() {
        let fixture = Fixture::new();
        let _server = FakeServer::unix(&fixture.daemon_socket, "codex-tui/0.163.0 (test)").await;
        let lifecycle = fixture.lifecycle(crate::config::ServerStartPolicy::Auto);
        let never_busy = || false;

        assert_eq!(
            lifecycle
                .ensure_attached(request(&never_busy))
                .await
                .expect("auto starts the daemon"),
            AttachOutcome::Attached
        );
        assert_eq!(fixture.cli.starts(), 1, "{:?}", fixture.cli.invocations());
        assert_eq!(fixture.probe_binaries.lock().unwrap().len(), 1);

        // A second pass is the steady state: same socket, the cached connection
        // still answers a read-only call, no redial and no management-CLI call.
        let connections = _server.connections();
        let invocations = fixture.cli.invocations().len();
        assert_eq!(
            lifecycle
                .ensure_attached(request(&never_busy))
                .await
                .expect("still attached"),
            AttachOutcome::Attached
        );
        assert_eq!(
            fixture.cli.invocations().len(),
            invocations,
            "a steady attachment runs no CLI: {:?}",
            fixture.cli.invocations()
        );
        assert_eq!(
            fixture.cli.starts(),
            1,
            "one daemon per absence, not one per pass: {:?}",
            fixture.cli.invocations()
        );
        _server.wait_for_liveness(1).await;
        assert_eq!(
            _server.connections(),
            connections,
            "a steady attachment proves liveness on the existing connection"
        );

        // cola never writes a pid record: it has no pid record to write, and
        // nothing under its state home exists after the attach.
        let pid_record = fixture.layout().self_spawned_pid();
        assert!(
            !pid_record.exists(),
            "cola must not record a daemon pid: {}",
            pid_record.display()
        );
        assert!(!pid_record.parent().expect("state dir").exists());

        let daemon_pids = fixture.daemon_pids();
        assert_eq!(daemon_pids.len(), 1, "exactly one daemon was started");
        let daemon_pid = daemon_pids[0];
        assert!(crate::process::alive(daemon_pid), "the daemon is running");

        drop(lifecycle);

        // The daemon is independent of the client that attached to it: its
        // process is alive and its socket still serves a fresh connection.
        assert!(
            crate::process::alive(daemon_pid),
            "dropping the client must not terminate the daemon"
        );
        let endpoint = Endpoint::Unix(fixture.environment.socket());
        crate::codex::transport::RpcConnection::open(&endpoint, Duration::from_secs(5))
            .await
            .expect("the daemon still serves after the client dropped");

        let log = fixture.cli.invocations();
        assert!(
            !log.iter()
                .any(|line| line.contains("daemon stop") || line.contains("daemon restart")),
            "cola never asks Codex to stop or restart the daemon: {log:?}"
        );
    }

    /// The review's case: a daemon that dies **without unlinking its socket**
    /// leaves the inode in place. Identity alone would keep reporting attached
    /// forever; the liveness probe must turn it into `Serverless`, and nothing
    /// is reaped.
    #[tokio::test]
    async fn a_dead_daemon_with_a_stale_socket_reads_unattached() {
        let fixture = Fixture::new();
        let server = FakeServer::unix(&fixture.daemon_socket, "codex-tui/0.163.0 (test)").await;
        let lifecycle = fixture.lifecycle(crate::config::ServerStartPolicy::Auto);
        let never_busy = || false;
        assert_eq!(
            lifecycle
                .ensure_attached(request(&never_busy))
                .await
                .expect("started and attached"),
            AttachOutcome::Attached
        );
        let daemon_pid = *fixture.daemon_pids().first().expect("daemon pid");
        assert_eq!(fixture.cli.starts(), 1);

        server.die().await;
        // The socket path is still there and still a socket — the corpse.
        assert!(
            fixture.environment.socket().exists(),
            "a crashed daemon leaves its path behind"
        );
        assert!(
            discovery::socket_identity(&fixture.environment.socket())
                .expect("identity")
                .is_some()
        );

        assert_eq!(
            lifecycle
                .ensure_attached(EnsureAttached {
                    allow_spawn: false,
                    heal_when_busy: false,
                    busy: &never_busy,
                    boot: false,
                })
                .await
                .expect("a stale socket is not an error"),
            AttachOutcome::Serverless,
            "same inode is not the same as an answering daemon"
        );
        assert_eq!(
            fixture.cli.starts(),
            1,
            "the poll pass never starts a daemon for a stale socket"
        );
        assert!(
            crate::process::alive(daemon_pid),
            "the daemon process was never killed"
        );
    }

    /// The review's other half: with **no cached attachment** at all, a stale
    /// socket path must not be read as a running daemon that is never started
    /// over. Discovery still reports the path, the dial proves it dead
    /// (`Unreachable`), and `auto` reaches the launcher.
    #[tokio::test]
    async fn auto_starts_over_a_stale_socket_without_a_cached_attachment() {
        let fixture = Fixture::new();
        let stale = FakeServer::unix(&fixture.daemon_socket, "codex-tui/0.163.0 (test)").await;
        stale.die().await;

        // A fresh lifecycle has never attached; the corpse is all it can see.
        std::fs::create_dir_all(fixture.environment.socket().parent().expect("control dir"))
            .expect("control dir");
        std::os::unix::fs::symlink(&fixture.daemon_socket, fixture.environment.socket())
            .expect("symlink the corpse");
        let replacement_path = fixture.root.path().join("daemon-1").join("app-server-control");
        std::fs::create_dir_all(replacement_path.parent().expect("dir")).expect("dir");
        let second = FakeServer::unix(&replacement_path, "codex-tui/0.163.0 (test)").await;
        fixture.cli.point_at(&replacement_path);

        let lifecycle = fixture.lifecycle(crate::config::ServerStartPolicy::Auto);
        let never_busy = || false;
        assert_eq!(
            lifecycle
                .ensure_attached(request(&never_busy))
                .await
                .expect("auto starts over the corpse"),
            AttachOutcome::Attached
        );
        assert_eq!(fixture.cli.starts(), 1);
        assert_eq!(second.connections(), 1, "the started daemon is attached");
    }

    /// `auto` reaches `daemon start` for a stale socket (it is not "attached" to
    /// a corpse), and attaches to the replacement once the launcher re-links the
    /// path. The old daemon is never touched.
    #[tokio::test]
    async fn auto_revives_a_stale_socket_through_the_launcher() {
        let fixture = Fixture::new();
        let first = FakeServer::unix(&fixture.daemon_socket, "codex-tui/0.163.0 (test)").await;
        let lifecycle = fixture.lifecycle(crate::config::ServerStartPolicy::Auto);
        let never_busy = || false;
        assert_eq!(
            lifecycle
                .ensure_attached(request(&never_busy))
                .await
                .expect("started and attached"),
            AttachOutcome::Attached
        );
        assert_eq!(fixture.cli.starts(), 1);
        let first_pid = *fixture.daemon_pids().first().expect("daemon pid");

        // The daemon dies, its socket path stays, and the launcher now links a
        // new daemon in: the same shape as `codex app-server daemon start`
        // replacing a crashed daemon's socket.
        first.die().await;
        let replacement_path = fixture.root.path().join("daemon-1").join("app-server-control");
        std::fs::create_dir_all(replacement_path.parent().expect("dir")).expect("dir");
        let second = FakeServer::unix(&replacement_path, "codex-tui/0.163.0 (test)").await;
        fixture.cli.point_at(&replacement_path);

        assert_eq!(
            lifecycle
                .ensure_attached(request(&never_busy))
                .await
                .expect("the launcher revives the daemon"),
            AttachOutcome::Attached
        );
        assert_eq!(fixture.cli.starts(), 2, "one revive, not a start per pass");
        assert_eq!(second.connections(), 1, "the replacement is dialled");
        assert!(
            crate::process::alive(first_pid),
            "the dead daemon's process was never killed"
        );
    }

    /// A control socket re-pointed **during** the handshake: the connection may
    /// belong to the old daemon, so recording the new identity would bind two
    /// servers into one attachment. The pass re-dials and stores the daemon the
    /// path actually names now.
    #[tokio::test]
    async fn a_replacement_during_the_handshake_voids_the_attach() {
        let fixture = Fixture::new();
        let first_path = fixture.root.path().join("daemon-0").join("app-server-control");
        std::fs::create_dir_all(first_path.parent().expect("dir")).expect("dir");
        let first = FakeServer::unix(&first_path, "codex-tui/0.163.0 (test)").await;
        let second_path = fixture.root.path().join("daemon-1").join("app-server-control");
        std::fs::create_dir_all(second_path.parent().expect("dir")).expect("dir");
        let second = FakeServer::unix(&second_path, "codex-tui/0.163.0 (test)").await;
        let control = fixture.environment.socket();
        std::fs::create_dir_all(control.parent().expect("control dir")).expect("control dir");
        std::os::unix::fs::symlink(&first_path, &control).expect("symlink");

        let seen = Arc::new(Mutex::new(Vec::new()));
        let lifecycle = CodexLifecycle::new(
            CodexConfig {
                start_server: crate::config::ServerStartPolicy::Never,
            },
            fixture.environment.clone(),
            Box::new(RepointingProbe {
                once: Arc::new(std::sync::Once::new()),
                control: control.clone(),
                target: second_path.clone(),
                seen: Arc::clone(&seen),
            }),
        );
        let never_busy = || false;
        assert_eq!(
            lifecycle
                .ensure_attached(request(&never_busy))
                .await
                .expect("the retry attaches to the daemon the path now names"),
            AttachOutcome::Attached
        );
        assert_eq!(first.connections(), 1, "the old daemon was dialled once");
        assert_eq!(second.connections(), 1, "the retry dialled the replacement");
        assert_eq!(
            seen.lock().expect("probe log").len(),
            2,
            "the retry builds its probe from the daemon it re-discovered, not the first one"
        );
        // The stored identity is the replacement: a steady pass probes *it*,
        // never the old connection.
        assert_eq!(
            lifecycle
                .ensure_attached(request(&never_busy))
                .await
                .expect("steady"),
            AttachOutcome::Attached
        );
        second.wait_for_liveness(1).await;
        assert_eq!(first.liveness_probes(), 0, "the voided connection is gone");
    }

    /// A dial that fails while the control path moves to a **healthy** daemon
    /// must be read as a replacement race, not as "no daemon": treating it as
    /// `Unreachable` would let `auto` run `daemon start` against the live
    /// replacement, whose idempotent start has no identity change to wait for.
    #[tokio::test]
    async fn a_failed_dial_that_raced_a_replacement_does_not_start() {
        let fixture = Fixture::new();
        // A's listener is gone but its socket path stays, so the dial fails
        // fast (`ECONNREFUSED`) at a path that still looks like a daemon.
        let stale = FakeServer::unix(&fixture.daemon_socket, "codex-tui/0.163.0 (test)").await;
        stale.die().await;
        let control = fixture.environment.socket();
        std::fs::create_dir_all(control.parent().expect("control dir")).expect("control dir");
        std::os::unix::fs::symlink(&fixture.daemon_socket, &control).expect("symlink the corpse");

        // The healthy daemon the path moves to while that dial fails.
        let replacement_path = fixture.root.path().join("daemon-1").join("app-server-control");
        std::fs::create_dir_all(replacement_path.parent().expect("dir")).expect("dir");
        let replacement = FakeServer::unix(&replacement_path, "codex-tui/0.163.0 (test)").await;
        fixture.cli.point_at(&replacement_path);

        let lifecycle = fixture.lifecycle(crate::config::ServerStartPolicy::Auto);
        let hook_control = control.clone();
        let hook_target = replacement_path.clone();
        lifecycle.set_dial_hook(Arc::new(move || {
            std::fs::remove_file(&hook_control).expect("remove the old link");
            std::os::unix::fs::symlink(&hook_target, &hook_control).expect("re-point the link");
        }));

        let never_busy = || false;
        assert_eq!(
            lifecycle
                .ensure_attached(request(&never_busy))
                .await
                .expect("the retry attaches to the daemon the path now names"),
            AttachOutcome::Attached
        );
        assert_eq!(
            fixture.cli.starts(),
            0,
            "a live replacement is never started over: {:?}",
            fixture.cli.invocations()
        );
        assert_eq!(
            replacement.connections(),
            1,
            "the healthy daemon the path names is dialled"
        );
    }

    /// A daemon that disappears is unattached on the next pass — and nothing is
    /// reaped, on any path (the poll pass included).
    #[tokio::test]
    async fn a_daemon_that_disappears_is_unattached_without_reaping() {
        let fixture = Fixture::new();
        let _server = FakeServer::unix(&fixture.daemon_socket, "codex-tui/0.163.0 (test)").await;
        let lifecycle = fixture.lifecycle(crate::config::ServerStartPolicy::Auto);
        let never_busy = || false;
        assert_eq!(
            lifecycle
                .ensure_attached(request(&never_busy))
                .await
                .expect("attached"),
            AttachOutcome::Attached
        );
        let daemon_pid = *fixture.daemon_pids().first().expect("daemon pid");

        // The daemon goes away and unlinks its control socket: the path is
        // gone, which discovery reads as absent.
        std::fs::remove_file(fixture.environment.socket()).expect("remove the control socket");

        // The poll path (no demand, no busy turn) and `never` both refuse to
        // start a replacement; both report unattached.
        assert_eq!(
            lifecycle
                .ensure_attached(EnsureAttached {
                    allow_spawn: false,
                    heal_when_busy: false,
                    busy: &never_busy,
                    boot: false,
                })
                .await
                .expect("absence is a state"),
            AttachOutcome::Serverless
        );
        assert_eq!(
            lifecycle
                .ensure_attached(request(&never_busy))
                .await
                .expect("absence is a state"),
            AttachOutcome::Attached,
            "auto starts a replacement when the demand path asks"
        );
        assert!(
            crate::process::alive(daemon_pid),
            "the old daemon was never killed"
        );
        assert!(!fixture.layout().self_spawned_pid().exists());
    }

    /// A daemon replaced behind the same control path (a restart re-points the
    /// symlink) is re-attached, not trusted by path alone.
    #[tokio::test]
    async fn a_replaced_daemon_is_reattached() {
        let fixture = Fixture::new();
        let first = FakeServer::unix(&fixture.daemon_socket, "codex-tui/0.163.0 (test)").await;
        let lifecycle = fixture.lifecycle(crate::config::ServerStartPolicy::Never);
        let never_busy = || false;
        // A daemon is already there: link the socket as the launcher would.
        std::fs::create_dir_all(fixture.environment.socket().parent().expect("control dir"))
            .expect("control dir");
        std::os::unix::fs::symlink(&fixture.daemon_socket, fixture.environment.socket()).expect("symlink");
        assert_eq!(
            lifecycle
                .ensure_attached(request(&never_busy))
                .await
                .expect("attached"),
            AttachOutcome::Attached
        );
        assert_eq!(first.connections(), 1);

        let replacement_path = fixture.root.path().join("daemon-1").join("app-server-control");
        std::fs::create_dir_all(replacement_path.parent().expect("dir")).expect("dir");
        let second = FakeServer::unix(&replacement_path, "codex-tui/0.163.0 (test)").await;
        std::fs::remove_file(fixture.environment.socket()).expect("remove link");
        std::os::unix::fs::symlink(&replacement_path, fixture.environment.socket())
            .expect("re-point the link");

        assert_eq!(
            lifecycle
                .ensure_attached(request(&never_busy))
                .await
                .expect("reattached"),
            AttachOutcome::Attached
        );
        assert_eq!(second.connections(), 1, "the replacement is dialled");
        assert_eq!(
            first.connections(),
            1,
            "the old connection is dropped, not re-used"
        );
    }

    /// The busy probe is read under the lifecycle lock, and a mid-turn heal
    /// (`allow_spawn = false`) may start a daemon only when a turn is in flight.
    #[tokio::test]
    async fn the_busy_probe_is_read_under_the_lock_and_gates_the_heal() {
        let fixture = Fixture::new();
        let _server = FakeServer::unix(&fixture.daemon_socket, "codex-tui/0.163.0 (test)").await;
        let lifecycle = fixture.lifecycle(crate::config::ServerStartPolicy::Auto);
        let called = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = Arc::clone(&called);
        let busy = move || {
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
            true
        };
        let request = EnsureAttached {
            allow_spawn: false,
            heal_when_busy: true,
            busy: &busy,
            boot: false,
        };

        // An idle poll (busy = false) never starts a daemon.
        let never_busy = || false;
        assert_eq!(
            lifecycle
                .ensure_attached(EnsureAttached {
                    allow_spawn: false,
                    heal_when_busy: true,
                    busy: &never_busy,
                    boot: false,
                })
                .await
                .expect("idle poll"),
            AttachOutcome::Serverless
        );
        assert_eq!(fixture.cli.starts(), 0);

        let guard = lifecycle.pass.lock().await;
        let mut pass = Box::pin(lifecycle.ensure_attached(request));
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut pass)
                .await
                .is_err(),
            "the pass must wait on the lifecycle lock"
        );
        assert!(
            !called.load(std::sync::atomic::Ordering::SeqCst),
            "the busy probe must not be read before the lock is acquired"
        );
        drop(guard);
        assert_eq!(
            pass.await.expect("the heal starts a daemon"),
            AttachOutcome::Attached
        );
        assert!(
            called.load(std::sync::atomic::Ordering::SeqCst),
            "the probe is read under the lock"
        );
        assert_eq!(fixture.cli.starts(), 1);
    }

    /// Restart is never offered: a live daemon is not owned, and no daemon
    /// means nothing to restart. Neither path runs a command.
    #[tokio::test]
    async fn restart_is_never_owned() {
        let fixture = Fixture::new();
        let lifecycle = fixture.lifecycle(crate::config::ServerStartPolicy::Never);
        assert_eq!(
            lifecycle.restart_server().await.expect("no daemon"),
            RestartOutcome::NoServer
        );
        assert!(fixture.cli.invocations().is_empty());

        let _server = FakeServer::unix(&fixture.daemon_socket, "codex-tui/0.163.0 (test)").await;
        std::fs::create_dir_all(fixture.environment.socket().parent().expect("dir")).expect("dir");
        std::os::unix::fs::symlink(&fixture.daemon_socket, fixture.environment.socket()).expect("symlink");
        assert_eq!(
            lifecycle.restart_server().await.expect("a daemon"),
            RestartOutcome::NotOwned
        );
        assert!(
            !fixture
                .cli
                .invocations()
                .iter()
                .any(|line| line.contains("stop") || line.contains("restart")),
            "{:?}",
            fixture.cli.invocations()
        );
    }

    /// ADR-0090 decision 3: a TUI on an embedded server shares nothing, and the
    /// lifecycle reports that instead of pretending an attachment exists.
    #[tokio::test]
    async fn an_embedded_server_client_is_diagnosed_not_mirrored() {
        let fixture = Fixture::new();
        let lifecycle = fixture.lifecycle(crate::config::ServerStartPolicy::Never);

        // A process the scan reads as `codex --no-daemon`: an `sh` symlink
        // named `codex` keeps its argv without an interpreter's re-exec.
        let tui_dir = fixture.root.path().join("tui");
        std::fs::create_dir_all(&tui_dir).expect("tui dir");
        let fake_tui = tui_dir.join("codex");
        std::os::unix::fs::symlink("/bin/sh", &fake_tui).expect("symlink");
        let mut child = std::process::Command::new(&fake_tui)
            .args(["-c", "sleep 30; :", "--no-daemon"])
            .spawn()
            .expect("spawn the fake TUI");
        let pid = child.id() as i32;
        wait_for_embedded(&lifecycle, pid).await;

        let never_busy = || false;
        assert_eq!(
            lifecycle
                .ensure_attached(request(&never_busy))
                .await
                .expect("no daemon to attach to"),
            AttachOutcome::Serverless
        );
        let _ = child.kill();
        let _ = child.wait();
    }

    /// The transition the process table cannot settle: with a daemon present, a
    /// TUI that names no `--remote` may or may not have joined it. Discovery
    /// reports it as **unverified**, never as sharing it cannot establish — and
    /// the lifecycle does not call it embedded either.
    #[tokio::test]
    async fn a_daemon_present_tui_is_unverified_not_assumed_sharing() {
        let fixture = Fixture::new();
        let _server = FakeServer::unix(&fixture.daemon_socket, "codex-tui/0.163.0 (test)").await;
        std::fs::create_dir_all(fixture.environment.socket().parent().expect("control dir"))
            .expect("control dir");
        std::os::unix::fs::symlink(&fixture.daemon_socket, fixture.environment.socket()).expect("symlink");
        let lifecycle = fixture.lifecycle(crate::config::ServerStartPolicy::Never);

        let tui_dir = fixture.root.path().join("tui");
        std::fs::create_dir_all(&tui_dir).expect("tui dir");
        let fake_tui = tui_dir.join("codex");
        std::os::unix::fs::symlink("/bin/sh", &fake_tui).expect("symlink");
        let mut child = std::process::Command::new(&fake_tui)
            .args(["-c", "sleep 30; :"])
            .spawn()
            .expect("spawn the fake TUI");
        let pid = child.id() as i32;

        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            let clients = lifecycle.diagnosis().await.expect("diagnosis").clients;
            if clients.unverified.iter().any(|client| client.pid == pid) {
                assert!(
                    clients.embedded.iter().all(|client| client.pid != pid),
                    "an unproven sharing state is not an embedded claim"
                );
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the fake TUI must be recorded as unverified: {clients:?}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let _ = child.kill();
        let _ = child.wait();
    }

    /// Wait until the process scan has seen `pid` as a proven embedded client.
    async fn wait_for_embedded(lifecycle: &CodexLifecycle, pid: i32) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            let clients = lifecycle.diagnosis().await.expect("diagnosis").clients;
            if clients.embedded.iter().any(|client| client.pid == pid) {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the fake TUI must be discovered as embedded: {clients:?}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Ticket #883 acceptance 4: `status`/`doctor` learn the active client's
    /// endpoint, its capabilities and its local clients through the same
    /// read-only report — never by naming the adapter. Linux-only: the
    /// attached claim comes from the descriptor proof.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn the_report_names_the_daemon_capabilities_and_an_attached_client() {
        // The descriptor proof needs `/proc` and `ss`; both are standard on
        // Linux. The pure proof has its own test in `discovery`, so a hidden
        // environment degrades this integration test to a skip, not a false
        // green.
        if std::process::Command::new("ss").arg("-x").output().is_err() {
            eprintln!("skipping descriptor proof: `ss` is unavailable");
            return;
        }
        let fixture = Fixture::new();
        let server = FakeServer::unix(&fixture.daemon_socket, "codex-tui/0.163.0 (test)").await;
        fixture.link_daemon();
        let lifecycle = fixture.lifecycle(crate::config::ServerStartPolicy::Never);
        let remote = format!("unix://{}", fixture.environment.socket().display());
        let Some(mut client) = fixture.spawn_connected_tui(&fixture.environment.socket(), &remote) else {
            eprintln!("skipping descriptor proof: `python3` is unavailable");
            return;
        };
        let pid = client.id() as i32;

        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        let report = loop {
            let report = lifecycle.client_report().await.expect("a report");
            if report
                .user_clients
                .iter()
                .any(|found| found.pid == pid && found.state == UserClientState::Attached)
            {
                break report;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the attached client must be discovered: {:?}",
                report.user_clients
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        let _ = client.kill();
        let _ = client.wait();

        assert_eq!(
            report.endpoint,
            format!("unix://{}", fixture.environment.socket().display())
        );
        assert_eq!(report.state, ClientState::Attached);
        assert_eq!(report.version.as_deref(), Some("0.163.0"));
        assert_eq!(report.platform.as_deref(), Some("unix/linux"));
        assert!(
            report
                .capabilities
                .iter()
                .any(|capability| capability.name == "background_tasks" && capability.value == "unsupported"),
            "the report states the missing Background Task surface: {:?}",
            report.capabilities
        );
        assert!(
            report
                .capabilities
                .iter()
                .any(|capability| capability.name == "surface"),
            "{:?}",
            report.capabilities
        );
        assert_eq!(
            report.client_binary.as_deref(),
            Some(fixture.cli.binary.display().to_string().as_str()),
            "the absolute client binary is reported as it would be spawned"
        );
        assert_eq!(
            report.runtime_binary.as_deref(),
            Some(
                fixture
                    .root
                    .path()
                    .join("managed-codex")
                    .display()
                    .to_string()
                    .as_str()
            ),
            "the daemon's own managed binary is reported"
        );
        assert!(server.connections() >= 1);
    }

    /// ADR-0090 decision 3: a TUI on an embedded server is a named failure in
    /// the report, not a healthy-looking attachment.
    #[tokio::test]
    async fn the_report_names_an_embedded_client() {
        let fixture = Fixture::new();
        let lifecycle = fixture.lifecycle(crate::config::ServerStartPolicy::Never);
        let mut client = fixture.spawn_tui(&[]);
        let pid = client.id() as i32;

        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        let report = loop {
            let report = lifecycle.client_report().await.expect("a report");
            if report
                .user_clients
                .iter()
                .any(|found| found.pid == pid && found.state == UserClientState::Embedded)
            {
                break report;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the embedded client must be discovered: {:?}",
                report.user_clients
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        let _ = client.kill();
        let _ = client.wait();

        assert_eq!(
            report.state,
            ClientState::Absent,
            "no daemon means no attachment: {:?}",
            report.state
        );
        let embedded = report
            .user_clients
            .iter()
            .find(|found| found.pid == pid)
            .expect("the embedded client is named");
        assert_eq!(embedded.state, UserClientState::Embedded);
        assert!(embedded.detail.contains("embedded"), "{}", embedded.detail);
    }

    /// Ticket #883 acceptance 3: a daemon-attached client plus a provider that
    /// completes a turn passes, and the scratch thread is cleaned up.
    #[tokio::test]
    async fn the_turn_probe_passes_and_deletes_its_scratch_thread() {
        let fixture = Fixture::new();
        let server = FakeServer::unix(&fixture.daemon_socket, "codex-tui/0.163.0 (test)").await;
        fixture.link_daemon();
        server.script_turn(
            "*",
            vec![json!({
                "method": "turn/completed",
                "params": { "turn": { "status": "completed" } },
            })],
        );
        let lifecycle = fixture.lifecycle(crate::config::ServerStartPolicy::Never);

        let outcome = lifecycle.probe_turn().await;
        assert!(matches!(outcome, TurnProbe::Passed { .. }), "{outcome:?}");
        assert_eq!(server.turn_starts(), 1, "one minimal turn");
        assert_eq!(
            server.deleted_threads().len(),
            1,
            "the probe thread is deleted: {:?}",
            server.deleted_threads()
        );
        let thread = server.deleted_threads()[0].clone();
        assert_eq!(server.subscribers(&thread), 0);
        // The prompt is the doctor's own diagnostic, not a user request.
        let params = server.turn_params(&thread);
        assert_eq!(params.len(), 1);
        assert!(
            params[0]["input"][0]["text"]
                .as_str()
                .is_some_and(|text| text.contains("cola doctor probe")),
            "{:?}",
            params[0]
        );
    }

    /// Ticket #883 acceptance 2: a turn with no usable credential is accepted
    /// and then fails as a **retryable** `error`. The probe must fail — a
    /// login-shaped check would pass while every turn died.
    #[tokio::test]
    async fn a_retryable_provider_error_fails_the_turn_probe() {
        let fixture = Fixture::new();
        let _server = FakeServer::unix(&fixture.daemon_socket, "codex-tui/0.163.0 (test)").await;
        fixture.link_daemon();
        _server.script_turn(
            "*",
            vec![json!({
                "method": "error",
                "params": {
                    "error": { "message": "401 Unauthorized" },
                    "willRetry": true,
                },
            })],
        );
        let lifecycle = fixture.lifecycle(crate::config::ServerStartPolicy::Never);

        let outcome = lifecycle.probe_turn().await;
        match outcome {
            TurnProbe::Failed { detail } => {
                assert!(detail.contains("will retry"), "{detail}");
                assert!(detail.contains("401"), "{detail}");
            }
            other => panic!("a retryable error must fail the probe, got {other:?}"),
        }
        assert_eq!(
            _server.deleted_threads().len(),
            1,
            "a failed probe still deletes its thread"
        );
    }

    /// Cancellation safety: dropping the probe future while it waits on the
    /// turn stream still interrupts the turn and deletes the scratch thread
    /// (the guard's `Drop` spawns the cleanup).
    #[tokio::test]
    async fn a_cancelled_turn_probe_still_cleans_up() {
        let fixture = Fixture::new();
        let server = FakeServer::unix(&fixture.daemon_socket, "codex-tui/0.163.0 (test)").await;
        fixture.link_daemon();
        let lifecycle = Arc::new(fixture.lifecycle(crate::config::ServerStartPolicy::Never));
        let task = tokio::spawn({
            let lifecycle = Arc::clone(&lifecycle);
            async move { lifecycle.probe_turn().await }
        });

        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while server.turn_starts() == 0 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the probe never started its turn"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        task.abort();
        let _ = task.await;

        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while server.deleted_threads().is_empty() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "a cancelled probe leaked its scratch thread"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            server.interrupt_params().len(),
            1,
            "cancellation stops the live probe turn"
        );
    }

    /// An ambiguous `thread/start` (the server created the thread, the reply
    /// was lost) has no id to delete. The cleanup finds it by the probe's
    /// unique directory and deletes it.
    #[tokio::test]
    async fn an_ambiguous_create_is_cleaned_up_by_its_probe_directory() {
        let fixture = Fixture::new();
        let server = FakeServer::unix(&fixture.daemon_socket, "codex-tui/0.163.0 (test)").await;
        fixture.link_daemon();
        let directory = fixture.root.path().join("ambiguous-probe");
        std::fs::create_dir_all(&directory).expect("probe dir");
        server.seed_thread(ScriptedThread::never_run(
            "thread-ambiguous",
            "session-root",
            &directory.to_string_lossy(),
        ));
        // A thread in another project must never be touched by the recovery.
        server.seed_thread(ScriptedThread::never_run(
            "thread-user",
            "session-root",
            "/tmp/user-project",
        ));
        let endpoint = Endpoint::Unix(fixture.environment.socket());
        let client = CodexClient::connect(&CodexOptions::new(endpoint), &PinnedProbe)
            .await
            .expect("attach to the scripted daemon");

        let result = clean_probe_thread(client.connection(), None, None, &directory).await;
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(
            server.deleted_threads(),
            vec!["thread-ambiguous".to_string()],
            "the by-directory lookup deleted the lost-response thread"
        );
    }

    /// The probe directory is reserved exclusively: a colliding name moves to
    /// the next suffix rather than adopting a pre-existing directory (whose
    /// threads recovery would otherwise be entitled to delete).
    #[test]
    fn the_probe_directory_is_reserved_exclusively() {
        let root = tempfile::tempdir().expect("tempdir");
        let pid = std::process::id();
        let seed = 42u128;
        let first = create_probe_directory_with(root.path(), pid, seed).expect("reserve");
        assert!(first.is_dir());
        let second = create_probe_directory_with(root.path(), pid, seed).expect("next suffix");
        assert_ne!(first, second);
        assert!(second.is_dir());
        // Occupy the next candidate name: the retry must skip it.
        std::fs::create_dir(root.path().join(format!("cola-doctor-{pid}-{}", seed + 2)))
            .expect("pre-existing candidate");
        let third = create_probe_directory_with(root.path(), pid, seed).expect("skips taken names");
        assert_ne!(third, first);
        assert_ne!(third, second);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&third).expect("metadata").permissions().mode() & 0o777,
                0o700,
                "the probe directory is private to this user"
            );
        }
        for directory in [first, second, third] {
            std::fs::remove_dir_all(directory).expect("cleanup");
        }
    }

    /// A passing provider whose cleanup failed is not a clean pass: the
    /// failure is in the result, and `doctor` turns it into a warning.
    #[tokio::test]
    async fn a_failed_cleanup_is_surfaced_in_a_passing_probe() {
        let fixture = Fixture::new();
        let server = FakeServer::unix(&fixture.daemon_socket, "codex-tui/0.163.0 (test)").await;
        fixture.link_daemon();
        server.script_turn(
            "*",
            vec![json!({
                "method": "turn/completed",
                "params": { "turn": { "status": "completed" } },
            })],
        );
        // Both the direct delete and the by-directory fallback fail.
        server.reject_thread_deletes(2);
        let lifecycle = fixture.lifecycle(crate::config::ServerStartPolicy::Never);

        match lifecycle.probe_turn().await {
            TurnProbe::Passed { cleanup, .. } => {
                let cleanup = cleanup.expect("the cleanup failure is surfaced");
                assert!(cleanup.contains("delete refused"), "{cleanup}");
            }
            other => panic!("the provider passed but the result hid cleanup: {other:?}"),
        }
    }

    /// With no daemon there is nothing to probe; the outcome says so rather
    /// than claiming a pass.
    #[tokio::test]
    async fn the_turn_probe_without_a_daemon_is_not_attempted() {
        let fixture = Fixture::new();
        let lifecycle = fixture.lifecycle(crate::config::ServerStartPolicy::Never);
        assert!(matches!(
            lifecycle.probe_turn().await,
            TurnProbe::NotAttempted { .. }
        ));
        assert!(
            fixture.cli.invocations().is_empty(),
            "a read-only probe never starts a daemon: {:?}",
            fixture.cli.invocations()
        );
    }

    /// The production probe asks for the managed binary, so the surface check
    /// can refuse a `codex` on PATH that is not the daemon's build.
    #[test]
    fn the_production_probe_targets_the_managed_binary() {
        let daemon = Daemon {
            socket: PathBuf::from("/tmp/app-server-control.sock"),
            managed_binary: PathBuf::from("/home/u/.codex/packages/app-server-daemon/current/bin/codex"),
            info: None,
        };
        let probe = ManagedProbe.probe(&daemon);
        assert!(
            probe
                .source()
                .contains("/home/u/.codex/packages/app-server-daemon/current/bin/codex"),
            "{}",
            probe.source()
        );
    }

    /// The seam's error is the neutral variant, labelled with this adapter.
    #[test]
    fn an_attach_failure_reports_through_the_neutral_variant() {
        let error = client_error(CodexError::Daemon("boom".into()));
        match error {
            BridgeError::Client { client, detail } => {
                assert_eq!(client, "codex");
                assert!(detail.contains("boom"), "{detail}");
            }
            other => panic!("expected the neutral client variant, got {other}"),
        }
    }

    /// The trait the seam stores is `Send + Sync`; this keeps the lifecycle
    /// usable from the poll loop's shared handles.
    #[test]
    fn the_lifecycle_is_send_and_sync() {
        fn requires<T: Send + Sync>() {}
        requires::<CodexLifecycle>();
    }

    /// Live: discover and attach to the daemon already running on this machine,
    /// through the binary that daemon actually runs.
    ///
    /// Opt-in like the repository's other live suites: set
    /// `COLA_LIVE_CODEX_HOME` (or an ambient `CODEX_HOME`) and run
    /// `cargo test -- --ignored live_`; it skips with a message when neither is
    /// set. Read-only by construction — `start_server = "never"` — so it never
    /// starts or stops the user's daemon. The hermetic tests above are the
    /// proof of the policy; this one validates the real socket, the symlink and
    /// the managed 0.164.0 binary the daemon runs.
    #[tokio::test]
    #[ignore = "live codex app-server daemon: set COLA_LIVE_CODEX_HOME or CODEX_HOME"]
    async fn live_attach_to_the_running_daemon() {
        let Some(home) = std::env::var_os("COLA_LIVE_CODEX_HOME")
            .or_else(|| std::env::var_os("CODEX_HOME"))
            .map(PathBuf::from)
        else {
            eprintln!("skipping live codex discovery: neither COLA_LIVE_CODEX_HOME nor CODEX_HOME is set");
            return;
        };
        let lifecycle = CodexLifecycle::new(
            CodexConfig {
                start_server: crate::config::ServerStartPolicy::Never,
            },
            Environment {
                home,
                cli: PathBuf::from("codex"),
                version_timeout: Duration::from_secs(10),
                start_timeout: Duration::from_secs(10),
                socket_ready_timeout: Duration::from_secs(2),
            },
            Box::new(ManagedProbe),
        );
        let never_busy = || false;
        let outcome = lifecycle
            .ensure_attached(request(&never_busy))
            .await
            .expect("the running daemon attaches");
        assert_eq!(outcome, AttachOutcome::Attached);
        // The steady-state pass proves the cached connection with a real
        // `thread/loaded/list` on the daemon's own state — no redial.
        assert_eq!(
            lifecycle
                .ensure_attached(request(&never_busy))
                .await
                .expect("the liveness probe answers on the real daemon"),
            AttachOutcome::Attached
        );
        assert_eq!(
            lifecycle.restart_server().await.expect("read"),
            RestartOutcome::NotOwned
        );
    }
}
