//! The app-server client: handshake, contract check and construction by the
//! active client's `kind` (spec #860, ticket #876).
//!
//! Construction order is deliberate:
//!
//! 1. dial the endpoint (WebSocket over `ws://` or a unix socket);
//! 2. `initialize`, read the server's identity, send `initialized`;
//! 3. ask the [`SurfaceProbe`] for that version's method surface;
//! 4. refuse if a required method is missing, naming it.
//!
//! Step 4 is the acceptance criterion: a server that cannot serve the adapter's
//! contract fails **here**, at construction, not at the first prompt.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};

use crate::bridge::tasks::TasksHandle;
use crate::config::{ActiveClient, ClientKind};

use super::backend::Projections;
use super::error::{CodexError, Result};
use super::schema::{PINNED_VERSION, SurfaceProbe};
use super::threads::Threads;
use super::transport::{Endpoint, RpcConnection};

/// How long the handshake (connect + `initialize` + `initialized`) may take.
pub(crate) const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// How to construct the client. The endpoint is supplied by the caller today;
/// daemon discovery under `$CODEX_HOME` lands with the lifecycle ticket (#877)
/// and fills the same field.
pub(crate) struct CodexOptions {
    pub(crate) endpoint: Endpoint,
    pub(crate) timeout: Duration,
    /// The Background Task State handle shared with the bridge. One per active
    /// client: the lifecycle creates it and hands the same instance to every
    /// client it attaches, so the Bridge records through the handle its reads
    /// go through (ADR-0079).
    pub(crate) tasks: TasksHandle,
}

impl CodexOptions {
    pub(crate) fn new(endpoint: Endpoint) -> Self {
        Self {
            endpoint,
            timeout: HANDSHAKE_TIMEOUT,
            tasks: TasksHandle::new(),
        }
    }

    /// Carry the active client's own task handle into the attached client.
    pub(crate) fn with_tasks(mut self, tasks: TasksHandle) -> Self {
        self.tasks = tasks;
        self
    }
}

/// The `codex-*` product version from an `initialize` `userAgent` (or from
/// `codex --version`'s `codex-cli 0.164.0`).
///
/// The user agent looks like
/// `codex-tui/0.164.0 (Arch Linux Unknown; x86_64) WindowsTerminal (cola; 0.1.0)`;
/// only the `codex-*` token is the server's version — the parenthesised client
/// version is not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ServerVersion {
    raw: String,
    pub(crate) parsed: semver::Version,
}

impl ServerVersion {
    pub(crate) fn parse(text: &str) -> Option<Self> {
        let tokens: Vec<&str> = text.split_whitespace().collect();
        for (index, token) in tokens.iter().enumerate() {
            let (name, inline) = match token.split_once('/') {
                Some((name, version)) => (name, Some(version)),
                None => (*token, None),
            };
            if !name.starts_with("codex") {
                continue;
            }
            let candidate = inline.or_else(|| tokens.get(index + 1).copied())?;
            let candidate = candidate.trim_matches(|character: char| {
                !(character.is_ascii_alphanumeric()
                    || character == '.'
                    || character == '-'
                    || character == '+')
            });
            if let Ok(parsed) = semver::Version::parse(candidate) {
                return Some(Self {
                    raw: candidate.to_string(),
                    parsed,
                });
            }
        }
        None
    }

    fn pinned() -> semver::Version {
        semver::Version::parse(PINNED_VERSION).expect("the pinned version is valid semver")
    }

    pub(crate) fn is_pinned(&self) -> bool {
        self.parsed == Self::pinned()
    }

    pub(crate) fn is_older_than_pin(&self) -> bool {
        self.parsed < Self::pinned()
    }
}

impl fmt::Display for ServerVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.raw)
    }
}

/// What the server told the adapter about itself, recorded on the client for
/// `cola status`/`doctor` (later tickets) and the attach log.
// The identity fields are read by the `cola status`/`doctor` rows (#883) and by
// tests; keeping them decoded is the point of recording `initialize`.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub(crate) struct ServerInfo {
    /// The raw `initialize` `userAgent`.
    pub(crate) user_agent: String,
    /// The binary version parsed out of it.
    pub(crate) version: ServerVersion,
    /// `initialize`'s `codexHome`, when present.
    pub(crate) codex_home: Option<String>,
    /// `initialize`'s `platformFamily`/`platformOs`, joined for display.
    pub(crate) platform: String,
    /// Where the verified surface came from (`pinned …` or `generated from …`).
    pub(crate) surface_source: String,
}

/// One attached app-server. Construction is the whole contract check; the
/// thread surface (#878) and the `Backend` implementation (#879) build on it.
///
/// Cloning shares the one connection, the thread registry, the read-model cache
/// and the task handle: a subscription is connection-scoped, so a clone must
/// address the same socket the handshake opened.
#[derive(Clone)]
pub(crate) struct CodexClient {
    /// Owned so a second connection is independent and the socket lives as long
    /// as the client does. Cloning the handle shares this one connection — the
    /// thread adapter needs the same socket the handshake opened, because a
    /// subscription is connection-scoped.
    connection: RpcConnection,
    /// Discovery and subscriptions over `connection`, with its own dispatcher
    /// for the push traffic a resumed thread produces.
    threads: Threads,
    /// The endpoint this connection dialled, for the neutral base-URL read.
    pub(crate) endpoint: Endpoint,
    /// The adapter-internal transcript cache (#879): subscriptions fold into
    /// it, and `Backend::transcript` reads it without waiting.
    pub(crate) projections: Arc<Projections>,
    /// The Background Task State handle, shared with the bridge.
    pub(crate) tasks: TasksHandle,
    server: ServerInfo,
}

impl fmt::Debug for CodexClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CodexClient")
            .field("server", &self.server)
            .finish_non_exhaustive()
    }
}

impl CodexClient {
    /// Connect, handshake, and verify the server's surface.
    pub(crate) async fn connect(options: &CodexOptions, probe: &dyn SurfaceProbe) -> Result<Self> {
        let connection = RpcConnection::open(&options.endpoint, options.timeout).await?;

        let initialize = tokio::time::timeout(
            options.timeout,
            connection.request(
                "initialize",
                json!({
                    "clientInfo": {
                        "name": "cola",
                        "version": env!("CARGO_PKG_VERSION"),
                    }
                }),
            ),
        )
        .await
        .map_err(|_| CodexError::Timeout(options.timeout))??;

        let user_agent = initialize
            .get("userAgent")
            .and_then(Value::as_str)
            .filter(|user_agent| !user_agent.trim().is_empty())
            .ok_or(CodexError::MissingUserAgent)?
            .to_string();
        let version = ServerVersion::parse(&user_agent)
            .ok_or_else(|| CodexError::UnparseableUserAgent(user_agent.clone()))?;

        // The protocol acknowledges the result before any further call; do it
        // even when the contract check below is about to fail, so a rejecting
        // server sees a well-formed client.
        connection.notify("initialized", None).await?;

        let surface = probe.surface(&version).await?;
        let missing = surface.missing_required();
        if !missing.is_empty() {
            return Err(CodexError::MissingContract {
                version: version.to_string(),
                missing: missing.join(", "),
                pinned: PINNED_VERSION.to_string(),
            });
        }
        let counts = surface.counts();
        let surface_source = probe.source();
        tracing::info!(
            version = %version,
            surface = %surface_source,
            client_requests = counts.0,
            notifications = counts.1,
            server_requests = counts.2,
            "attached to codex app-server"
        );

        // Start the status consumer before the thread dispatcher can consume the
        // same broadcast: `thread/status/changed` is server-broadcast, so
        // Session Sync must read it even for a thread this connection never
        // resumes.
        let projections = Arc::new(Projections::default());
        projections.watch_thread_status(&connection);

        Ok(Self {
            threads: Threads::new(connection.clone()),
            connection,
            endpoint: options.endpoint.clone(),
            projections,
            tasks: options.tasks.clone(),
            server: ServerInfo {
                user_agent,
                version,
                codex_home: initialize
                    .get("codexHome")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                platform: platform(&initialize),
                surface_source,
            },
        })
    }

    pub(crate) fn server_info(&self) -> &ServerInfo {
        &self.server
    }

    /// The live connection, shared by the lifecycle's probe and the thread
    /// adapter. Clones address the same socket.
    pub(crate) fn connection(&self) -> &RpcConnection {
        &self.connection
    }

    /// The thread discovery and subscription surface over this connection.
    pub(crate) fn threads(&self) -> &Threads {
        &self.threads
    }
}

fn platform(initialize: &Value) -> String {
    let field = |name: &str| initialize.get(name).and_then(Value::as_str);
    match (field("platformFamily"), field("platformOs")) {
        (Some(family), Some(os)) => format!("{family}/{os}"),
        (Some(family), None) => family.to_string(),
        (None, Some(os)) => os.to_string(),
        (None, None) => "unknown".to_string(),
    }
}

/// Construct the adapter the active client's `kind` selects.
///
/// The id is deliberately not consulted: a `[clients.<id>]` rename is a state
/// migration, not a new adapter (ADR-0090 decision 5/6). `Ok(None)` means the
/// active client is not a Codex client and the caller keeps its own adapter.
// A pure selection helper kept beside the construction it documents (and its
// own test): the composition root now matches on `kind` and builds the
// lifecycle, so this pre-lifecycle entry point has no production caller.
#[allow(dead_code)]
pub(crate) async fn connect_for_active_client(
    active: &ActiveClient,
    options: &CodexOptions,
    probe: &dyn SurfaceProbe,
) -> Result<Option<CodexClient>> {
    match active.kind {
        ClientKind::Opencode => Ok(None),
        ClientKind::Codex => CodexClient::connect(options, probe).await.map(Some),
    }
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;

    use super::*;
    use crate::codex::schema::{PinnedProbe, Surface};
    use crate::codex::test_server::FakeServer;

    struct FixedProbe(Surface);

    #[async_trait]
    impl SurfaceProbe for FixedProbe {
        async fn surface(&self, _server: &ServerVersion) -> Result<Surface> {
            Ok(self.0.clone())
        }

        fn source(&self) -> String {
            "test fixture".to_string()
        }
    }

    async fn server(user_agent: &str) -> (FakeServer, Endpoint) {
        let (server, url) = FakeServer::tcp(user_agent).await;
        (server, Endpoint::parse(&url).expect("endpoint"))
    }

    async fn connect(user_agent: &str, probe: &dyn SurfaceProbe) -> Result<CodexClient> {
        let (_server, endpoint) = server(user_agent).await;
        CodexClient::connect(&CodexOptions::new(endpoint), probe).await
    }

    #[test]
    fn the_version_comes_from_the_codex_product_not_the_client_parenthesis() {
        let version = ServerVersion::parse(
            "codex-tui/0.164.0 (Arch Linux Unknown; x86_64) WindowsTerminal (cola; 0.0.1)",
        )
        .expect("version");
        assert_eq!(version.to_string(), "0.164.0");
        assert!(!version.is_pinned());
        assert!(
            ServerVersion::parse("codex-cli 0.163.0")
                .expect("cli")
                .is_pinned()
        );
        assert!(ServerVersion::parse("codex-tui/not-a-version").is_none());
    }

    /// The acceptance criterion: a server that lacks a required method fails at
    /// construction with the method named.
    #[tokio::test]
    async fn a_missing_required_method_fails_at_construction_naming_it() {
        let mut surface = Surface::pinned();
        surface.client_requests.remove("turn/steer");
        let error = connect("codex-tui/0.163.0 (test)", &FixedProbe(surface))
            .await
            .expect_err("construction must fail");
        let message = error.to_string();
        assert!(message.contains("turn/steer"), "{message}");
        assert!(message.contains("0.163.0"), "{message}");
    }

    /// A newer server passes only after its own surface has been checked — the
    /// version is recorded, never guessed.
    #[tokio::test]
    async fn a_newer_server_passes_with_a_verified_surface() {
        let client = connect("codex-tui/0.164.0 (test)", &FixedProbe(Surface::pinned()))
            .await
            .expect("a verified newer server passes");
        assert_eq!(client.server_info().version.to_string(), "0.164.0");
        assert_eq!(client.server_info().surface_source, "test fixture");
    }

    /// With only the pinned surface known, a newer or older server is refused
    /// loudly rather than guessed at.
    #[tokio::test]
    async fn an_unknown_version_degrades_loudly() {
        for (user_agent, needle) in [
            ("codex-tui/0.164.0 (test)", "0.164.0"),
            ("codex-tui/0.162.0 (test)", "0.162.0"),
        ] {
            let error = connect(user_agent, &PinnedProbe).await.expect_err("refused");
            let message = error.to_string();
            assert!(message.contains(needle), "{message}");
        }
    }

    #[tokio::test]
    async fn an_initialize_result_without_a_user_agent_is_rejected() {
        let error = connect("", &PinnedProbe)
            .await
            .expect_err("an unidentified server is refused");
        assert!(matches!(error, CodexError::MissingUserAgent), "{error}");
    }

    #[tokio::test]
    async fn an_initialize_error_is_surfaced() {
        let (_server, url) = FakeServer::tcp_rejecting("not a codex client").await;
        let endpoint = Endpoint::parse(&url).expect("endpoint");
        let error = CodexClient::connect(&CodexOptions::new(endpoint), &PinnedProbe)
            .await
            .expect_err("an initialize error fails construction");
        match error {
            CodexError::Rpc { method, detail, .. } => {
                assert_eq!(method, "initialize");
                assert!(detail.contains("not a codex client"), "{detail}");
            }
            other => panic!("expected an RPC error, got {other}"),
        }
    }

    /// End to end through the real probe: a newer server whose *generated*
    /// surface lacks a required method fails at construction, naming it.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_generated_surface_missing_a_method_fails_at_construction() {
        use std::os::unix::fs::PermissionsExt;

        use crate::codex::schema::{
            AutoProbe, REQUIRED_CLIENT_METHODS, REQUIRED_NOTIFICATIONS, REQUIRED_SERVER_REQUESTS,
            write_generated_surface,
        };
        use crate::codex::test_server::shell_single_quote;

        let directory = tempfile::tempdir().expect("tempdir");
        // The apostrophe is deliberate: it proves the fixture path is quoted
        // for the shell, not just concatenated between two quote characters.
        let fixture = directory.path().join("schema's-fixture");
        std::fs::create_dir(&fixture).expect("schema fixture dir");
        let client_methods: Vec<&str> = REQUIRED_CLIENT_METHODS
            .iter()
            .copied()
            .filter(|method| *method != "turn/steer")
            .collect();
        write_generated_surface(
            &fixture,
            &client_methods,
            REQUIRED_NOTIFICATIONS,
            REQUIRED_SERVER_REQUESTS,
        )
        .expect("write schema fixture");

        let binary = directory.path().join("codex");
        std::fs::write(
            &binary,
            format!(
                "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo \"codex-cli 0.164.0\"; exit 0; fi\ncp {}/*.json \"$4\"/\n",
                shell_single_quote(&fixture.display().to_string())
            ),
        )
        .expect("write fake codex");
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755))
            .expect("make fake codex executable");

        let (_server, endpoint) = server("codex-tui/0.164.0 (test)").await;
        let error = CodexClient::connect(&CodexOptions::new(endpoint), &AutoProbe::new(&binary))
            .await
            .expect_err("a missing generated method fails construction");
        match error {
            CodexError::MissingContract { missing, .. } => {
                assert!(missing.contains("turn/steer"), "{missing}");
            }
            other => panic!("expected a missing contract, got {other}"),
        }
    }

    /// The adapter is selected by `kind`, never by the configured id.
    #[tokio::test]
    async fn the_adapter_is_selected_by_kind_not_id() {
        let opencode = active("grace", ClientKind::Opencode);
        let (_server, endpoint) = server("codex-tui/0.163.0 (test)").await;
        let options = CodexOptions::new(endpoint);
        assert!(
            connect_for_active_client(&opencode, &options, &PinnedProbe)
                .await
                .expect("an opencode client is simply not this adapter")
                .is_none()
        );

        for id in ["ada", "grace"] {
            let codex = active(id, ClientKind::Codex);
            let client = connect_for_active_client(&codex, &options, &PinnedProbe)
                .await
                .expect("a codex client constructs")
                .expect("this adapter is selected");
            assert_eq!(client.server_info().version.to_string(), "0.163.0");
        }
    }

    fn active(id: &str, kind: ClientKind) -> ActiveClient {
        ActiveClient {
            id: id.to_string(),
            kind,
            settings: match kind {
                ClientKind::Opencode => {
                    crate::config::ClientSettings::Opencode(crate::config::OpenCodeConfig::default())
                }
                ClientKind::Codex => {
                    crate::config::ClientSettings::Codex(crate::config::CodexConfig::default())
                }
            },
        }
    }

    /// Live: `initialize` + `initialized` against a real daemon, twice.
    ///
    /// Opt-in like the repository's other live suites: set
    /// `COLA_LIVE_CODEX_SOCKET` (the control socket, symlink allowed) and
    /// `COLA_LIVE_CODEX_BIN` (the binary the daemon runs, for the schema
    /// probe) and run `cargo test -- --ignored codex::client::tests::live_`
    /// (or `--ignored live_initialize`). It skips with a message when the
    /// environment is absent.
    ///
    /// Unix-only like the transport it exercises: on a platform without unix
    /// domain sockets the endpoint construction is the named unsupported-
    /// platform error, not a skipped run.
    #[cfg(unix)]
    #[tokio::test]
    #[ignore = "live codex app-server: set COLA_LIVE_CODEX_SOCKET and COLA_LIVE_CODEX_BIN"]
    async fn live_initialize_then_initialized_and_a_second_connection() {
        let Some(socket) = std::env::var_os("COLA_LIVE_CODEX_SOCKET") else {
            eprintln!("skipping live codex test: COLA_LIVE_CODEX_SOCKET is unset");
            return;
        };
        let Some(binary) = std::env::var_os("COLA_LIVE_CODEX_BIN") else {
            eprintln!("skipping live codex test: COLA_LIVE_CODEX_BIN is unset");
            return;
        };
        let endpoint = Endpoint::parse(&format!("unix://{}", std::path::Path::new(&socket).display()))
            .expect("endpoint");
        let probe = crate::codex::schema::AutoProbe::new(binary);

        let first = CodexClient::connect(&CodexOptions::new(endpoint.clone()), &probe)
            .await
            .expect("first live connection");
        let second = CodexClient::connect(&CodexOptions::new(endpoint), &probe)
            .await
            .expect("second live connection");

        assert_eq!(
            first.server_info().user_agent,
            second.server_info().user_agent,
            "both connections identify the same daemon"
        );
        // The connection is live after the handshake: a real call answers.
        let result = first
            .connection()
            .request("thread/loaded/list", json!({}))
            .await
            .expect("a call after initialize answers");
        assert!(result.is_object() || result.is_array(), "unexpected {result}");
    }
}
