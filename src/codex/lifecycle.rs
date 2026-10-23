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
//! A local client on an *embedded* server is diagnosed rather than mirrored
//! (ADR-0090 decision 3): the pass logs it once and [`CodexLifecycle::diagnosis`]
//! reports it for `cola doctor`, while the attach outcome stays `Serverless` —
//! there is no daemon to attach to, and cola will not pretend otherwise.
//!
//! The `Backend` implementation that serves Sessions is a later ticket (#879);
//! this type is the lifecycle half it will build on.

use std::sync::atomic::{AtomicI32, Ordering};

use async_trait::async_trait;

use crate::backend::lifecycle::{AttachOutcome, EnsureAttached, RestartOutcome, ServerLifecycle};
use crate::config::CodexConfig;
use crate::error::BridgeError;

use super::client::{CodexClient, CodexOptions};
use super::discovery::{self, Daemon, DaemonState, Discovery, Environment, SocketIdentity};
use super::error::{CodexError, Result};
use super::schema::{AutoProbe, SurfaceProbe};
use super::transport::Endpoint;

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

/// The active Codex Agent Client's lifecycle.
///
/// Holds the live connection between passes so the poll loop does not redial a
/// daemon that is still there; a replaced socket is re-attached, and a vanished
/// one is reported unattached on the next pass.
pub(crate) struct CodexLifecycle {
    settings: CodexConfig,
    environment: Environment,
    probes: Box<dyn ProbeSource>,
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
}

impl CodexLifecycle {
    pub(crate) fn new(settings: CodexConfig, environment: Environment, probes: Box<dyn ProbeSource>) -> Self {
        Self {
            settings,
            environment,
            probes,
            attached: tokio::sync::Mutex::new(None),
            pass: tokio::sync::Mutex::new(()),
            reported_embedded: AtomicI32::new(0),
        }
    }

    /// What discovery last observed — the read `cola doctor` needs for ADR-0090
    /// decision 3 (daemon presence, the binary it runs, embedded clients).
    /// No caller until #883.
    #[allow(dead_code)]
    pub(crate) async fn diagnosis(&self) -> Result<Discovery> {
        discovery::discover(&self.environment).await
    }

    /// One attach to a discovered daemon.
    ///
    /// A transport-level failure means the daemon went away between the socket
    /// check and the dial: it is reported as unattached and retried on the next
    /// pass, never reaped. A daemon that answers and is unusable (a contract or
    /// probe failure) stays loud — retrying cannot fix it.
    async fn attach(&self, daemon: &Daemon) -> Result<AttachOutcome> {
        let probe = self.probes.probe(daemon);
        let options = CodexOptions::new(Endpoint::Unix(daemon.socket.clone()));
        match CodexClient::connect(&options, probe.as_ref()).await {
            Ok(client) => {
                let identity = discovery::socket_identity(&daemon.socket)?.ok_or_else(|| {
                    CodexError::Daemon(format!(
                        "the control socket at {} vanished during the attach",
                        daemon.socket.display()
                    ))
                })?;
                tracing::info!(
                    socket = %daemon.socket.display(),
                    version = %client.server_info().version,
                    surface = %client.server_info().surface_source,
                    "attached to the codex app-server daemon"
                );
                *self.attached.lock().await = Some(Attachment { client, identity });
                Ok(AttachOutcome::Attached)
            }
            Err(error) if daemon_gone(&error) => {
                tracing::warn!(
                    "the codex daemon at {} is gone or unreachable; staying unattached: {}",
                    daemon.socket.display(),
                    error
                );
                Ok(AttachOutcome::Serverless)
            }
            Err(error) => Err(error),
        }
    }

    /// Log a local client that runs its own app-server, once per client, rather
    /// than letting cola mirror nothing in silence (ADR-0090 decision 3).
    fn report_embedded(&self, discovered: &Discovery) {
        let Some(client) = discovered.embedded.first() else {
            return;
        };
        if self.reported_embedded.swap(client.pid, Ordering::Relaxed) == client.pid {
            return;
        }
        tracing::warn!(
            "a local Codex client (pid {}, {}) is running on an embedded app-server; it shares no \
             thread with the shared daemon, so cola cannot mirror it — start the daemon (and attach \
             the client to it) for Peer Attachment",
            client.pid,
            client.evidence
        );
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

        let discovered = discovery::discover(&self.environment)
            .await
            .map_err(client_error)?;
        self.report_embedded(&discovered);

        // The same daemon on the same socket: stay attached. Comparative
        // identity, not just the path — a restarted daemon re-points its
        // symlink, and a new inode means a new server behind it.
        if let Some(identity) = discovered.socket
            && self
                .attached
                .lock()
                .await
                .as_ref()
                .is_some_and(|attachment| attachment.identity == identity)
        {
            return Ok(AttachOutcome::Attached);
        }
        // Gone or replaced: drop the stale connection. Nothing is sent to the
        // daemon — it is not cola's to stop.
        self.attached.lock().await.take();

        if let DaemonState::Running(daemon) = &discovered.daemon {
            return self.attach(daemon).await.map_err(client_error);
        }

        // No daemon. `never` is enforced here, on every path, which is why the
        // policy is checked before the request's allowances: a demand-path or
        // mid-turn-heal start can never override it.
        let want_start = self.settings.start_server.spawns_when_needed()
            && (request.allow_spawn || (request.heal_when_busy && (request.busy)()));
        if !want_start {
            return Ok(AttachOutcome::Serverless);
        }

        discovery::start(&self.environment).await.map_err(client_error)?;
        tracing::info!("started the codex app-server daemon through its own CLI (`app-server daemon start`)");
        let daemon = match discovery::discover(&self.environment)
            .await
            .map_err(client_error)?
            .daemon
        {
            DaemonState::Running(daemon) => daemon,
            DaemonState::Absent { evidence } => {
                return Err(client_error(CodexError::Daemon(format!(
                    "the daemon was started but is not discoverable: {evidence}"
                ))));
            }
        };
        self.attach(&daemon).await.map_err(client_error)
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
    use crate::codex::test_server::{FakeCodex, FakeServer};

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

        // A second pass is the fast path: already attached to the same socket.
        assert_eq!(
            lifecycle
                .ensure_attached(request(&never_busy))
                .await
                .expect("still attached"),
            AttachOutcome::Attached
        );
        assert_eq!(
            fixture.cli.starts(),
            1,
            "one daemon per absence, not one per pass: {:?}",
            fixture.cli.invocations()
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
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            let found = lifecycle.diagnosis().await.expect("diagnosis").embedded;
            if found.iter().any(|client| client.pid == pid) {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the fake TUI must be discovered: {found:?}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

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
        assert_eq!(
            lifecycle.restart_server().await.expect("read"),
            RestartOutcome::NotOwned
        );
    }
}
