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

use serde::Deserialize;
use serde_json::{Value, json};

use crate::bridge::tasks::TasksHandle;
use crate::config::{ActiveClient, ClientKind};

use super::backend::{Projections, SkillCatalog};
use super::error::{CodexError, Result};
use super::schema::{PINNED_VERSION, SurfaceProbe};
use super::threads::{ThreadSettings, Threads};
use super::transport::{Endpoint, RpcConnection};

/// How long the handshake (connect + `initialize` + `initialized`) may take.
pub(crate) const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// The daemon-side feature that makes the model send
/// `item/tool/requestUserInput` instead of asking in plain text.
///
/// It is a **server config** feature (`[features]
/// default_mode_request_user_input = true` at daemon/TUI start), not a client
/// capability: the request reaches a client that did not opt into the
/// experimental API, and the client's answer is accepted.
pub(crate) const AGENT_QUESTIONS_FEATURE: &str = "default_mode_request_user_input";

/// The page size `experimentalFeature/list` is asked for.
const EXPERIMENTAL_FEATURE_PAGE_SIZE: u32 = 200;

/// Bound the feature-list walk. The list is small and the method is
/// experimental; a server that keeps minting cursors must not hold the report.
const MAX_EXPERIMENTAL_FEATURE_PAGES: usize = 8;

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
    /// The record of turns cola started (ADR-0090's authorship consequence).
    /// The daemon hands the durable, client-scoped record; a bare client gets
    /// an in-memory one, which is still correct for its own lifetime.
    pub(crate) authored: Arc<super::authorship::Authorship>,
    /// The sandbox/approval overrides this client's threads are asked to run
    /// under (issue #898). `Default` sends nothing, so a bare client keeps the
    /// daemon's own defaults.
    pub(crate) thread_settings: ThreadSettings,
    /// The next-turn pending selections shared with every other attachment the
    /// lifecycle builds (spec #922). A bare client leaves it `None` and gets
    /// its own store.
    pub(crate) pending: Option<Arc<super::backend::PendingSelections>>,
}

impl CodexOptions {
    pub(crate) fn new(endpoint: Endpoint) -> Self {
        Self {
            endpoint,
            timeout: HANDSHAKE_TIMEOUT,
            tasks: TasksHandle::new(),
            authored: super::authorship::Authorship::in_memory(),
            thread_settings: ThreadSettings::default(),
            pending: None,
        }
    }

    /// Carry the active client's own task handle into the attached client.
    pub(crate) fn with_tasks(mut self, tasks: TasksHandle) -> Self {
        self.tasks = tasks;
        self
    }

    /// Carry the durable authorship record into the attached client.
    pub(crate) fn with_authorship(mut self, authored: Arc<super::authorship::Authorship>) -> Self {
        self.authored = authored;
        self
    }

    /// Carry the active client's per-thread sandbox/approval overrides into
    /// this connection (issue #898).
    pub(crate) fn with_thread_settings(mut self, settings: ThreadSettings) -> Self {
        self.thread_settings = settings;
        self
    }

    /// Carry the lifecycle's in-memory pending store into this connection
    /// (spec #922), so a replacement attachment keeps a pick the user made.
    pub(crate) fn with_pending(mut self, pending: Arc<super::backend::PendingSelections>) -> Self {
        self.pending = Some(pending);
        self
    }
}

/// The app-server version from an `initialize` `userAgent` (or from
/// `codex --version`'s `codex-cli 0.164.0`).
///
/// The user agent's **leading** token is `<client name>/<app-server version>`:
/// the observed live daemon sends
/// `cola/0.164.0 (Arch Linux Unknown; x86_64) WindowsTerminal (cola; 0.14.2)`,
/// where `0.164.0` is the server and the parenthetical `(cola; 0.14.2)` is the
/// **client's own** version, never the server's. A `codex-*`-named
/// `name/version` token is preferred wherever it appears — a TUI-originated
/// `codex-tui/0.164.0 …` keeps its old meaning — and otherwise the first token
/// whose slash-separated version is valid semver wins, whatever the client
/// calls itself. The older space-separated `codex-cli 0.163.0` shape is kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ServerVersion {
    raw: String,
    pub(crate) parsed: semver::Version,
}

impl ServerVersion {
    pub(crate) fn parse(text: &str) -> Option<Self> {
        let tokens: Vec<&str> = text.split_whitespace().collect();
        // Two passes over `name/version` tokens: a `codex-*` name wins wherever
        // it appears, so a TUI-originated UA keeps its old meaning; otherwise
        // the leading token's version is the server's, whatever the client
        // calls itself. A parenthetical client version is never reached: it is
        // a separate space-delimited token with no slash.
        for require_codex in [true, false] {
            for token in &tokens {
                let Some((name, candidate)) = token.split_once('/') else {
                    continue;
                };
                if require_codex && !name.starts_with("codex") {
                    continue;
                }
                if let Some(version) = Self::from_candidate(candidate) {
                    return Some(version);
                }
            }
        }
        // The older CLI shape has no slash: `codex-cli 0.163.0`.
        for (index, token) in tokens.iter().enumerate() {
            if !token.starts_with("codex") {
                continue;
            }
            if let Some(candidate) = tokens.get(index + 1)
                && let Some(version) = Self::from_candidate(candidate)
            {
                return Some(version);
            }
        }
        None
    }

    /// Parse one candidate version, trimming the protocol's punctuation (a
    /// parenthesised or comma-terminated token keeps its semver intact).
    fn from_candidate(candidate: &str) -> Option<Self> {
        let candidate = candidate.trim_matches(|character: char| {
            !(character.is_ascii_alphanumeric() || character == '.' || character == '-' || character == '+')
        });
        let parsed = semver::Version::parse(candidate).ok()?;
        Some(Self {
            raw: candidate.to_string(),
            parsed,
        })
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
/// `cola status`/`doctor` (#883) and the attach log.
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
    /// The three verified method-list sizes (client → server, notifications,
    /// server → client), for `cola status`/`doctor`.
    pub(crate) surface_counts: (usize, usize, usize),
}

/// One `experimentalFeature/list` page. The method is experimental: a server
/// that does not serve it answers an RPC error, which the caller treats as
/// unknown rather than as a failed report.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExperimentalFeatureListResponse {
    #[serde(default)]
    data: Vec<ExperimentalFeature>,
    #[serde(default)]
    next_cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ExperimentalFeature {
    name: String,
    /// Present in the pinned schema; kept optional so a malformed row is
    /// "unknown", never a false `off`.
    #[serde(default)]
    enabled: Option<bool>,
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
    /// The server-initiated approvals and questions this connection received
    /// (#882). One record per connection: an approval reaches every
    /// subscriber, and this is where one connection's copy waits for an
    /// answer.
    pub(crate) approvals: Arc<super::approvals::Approvals>,
    /// The Background Task State handle, shared with the bridge.
    pub(crate) tasks: TasksHandle,
    /// The turns cola started, so the External Message question is answered
    /// from cola's own record (ADR-0090).
    pub(crate) authored: Arc<super::authorship::Authorship>,
    /// The most recent `skills/list` read, for the `/skill <id>` prompt part:
    /// Codex's skill input needs the skill's `path`, which the neutral
    /// [`crate::backend::PromptSkill`] does not carry.
    pub(crate) skills: Arc<SkillCatalog>,
    /// Per-provider `GET <base_url>/models` answers (ticket #908). Shared by
    /// clones so re-opening `/model` on one connection reuses what it already
    /// learned, including failures.
    pub(crate) model_probes: Arc<super::model_surface::ModelProbes>,
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
        let projections = Arc::new(Projections::default().with_pending(options.pending.clone()));
        projections.watch_thread_status(&connection);

        // Record server-initiated requests before any subscription can produce
        // one: an approval is answered on this connection's inbound stream, and
        // a missed request would leave the server waiting. The session-decision
        // set rides with it: the card offers "always" only where this server's
        // own response schemas declare the variant (ticket #882).
        let approvals = Arc::new(super::approvals::Approvals::new(surface.session_decisions()));
        approvals.watch(&connection, &projections);

        Ok(Self {
            threads: Threads::new(connection.clone(), options.thread_settings),
            connection,
            endpoint: options.endpoint.clone(),
            projections,
            approvals,
            tasks: options.tasks.clone(),
            authored: options.authored.clone(),
            skills: Arc::new(SkillCatalog::default()),
            model_probes: Arc::new(super::model_surface::ModelProbes::new()),
            server: ServerInfo {
                user_agent,
                version,
                codex_home: initialize
                    .get("codexHome")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                platform: platform(&initialize),
                surface_source,
                surface_counts: counts,
            },
        })
    }

    pub(crate) fn server_info(&self) -> &ServerInfo {
        &self.server
    }

    /// Whether the daemon-side agent-question feature is on, read from
    /// `experimentalFeature/list`.
    ///
    /// `Ok(Some(true|false))` is the listed feature's `enabled`; `Ok(None)` means
    /// the feature is not listed (or its row carries no `enabled` value), which
    /// the caller reports as unknown. The method is **experimental** and not in
    /// the required surface: an RPC error from a server that does not serve it
    /// is the caller's to interpret as best-effort, so it is returned as `Err`
    /// rather than hidden here.
    pub(crate) async fn agent_questions_enabled(&self) -> Result<Option<bool>> {
        let mut cursor: Option<String> = None;
        let mut seen = std::collections::HashSet::new();
        for _ in 0..MAX_EXPERIMENTAL_FEATURE_PAGES {
            let mut params = json!({ "limit": EXPERIMENTAL_FEATURE_PAGE_SIZE });
            if let Some(cursor) = cursor.as_deref() {
                params["cursor"] = json!(cursor);
            }
            let result = self
                .connection
                .request("experimentalFeature/list", params)
                .await?;
            let page: ExperimentalFeatureListResponse =
                serde_json::from_value(result).map_err(|error| CodexError::Protocol {
                    method: "experimentalFeature/list".to_string(),
                    detail: error.to_string(),
                })?;
            for feature in page.data {
                if feature.name == AGENT_QUESTIONS_FEATURE {
                    return Ok(feature.enabled);
                }
            }
            match page.next_cursor {
                None => return Ok(None),
                Some(next) => {
                    if !seen.insert(next.clone()) {
                        tracing::warn!(
                            "codex experimentalFeature/list repeated cursor `{next}`; \
                             the agent-question feature state is unknown"
                        );
                        return Ok(None);
                    }
                    cursor = Some(next);
                }
            }
        }
        tracing::warn!(
            "codex experimentalFeature/list did not finish in {MAX_EXPERIMENTAL_FEATURE_PAGES} \
             pages; the agent-question feature state is unknown"
        );
        Ok(None)
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
    fn the_version_comes_from_the_leading_server_token_not_the_client_parenthesis() {
        // The observed live daemon: the client names itself in the leading
        // token and again in the parenthetical. Only the leading token's
        // version is the server's; `0.14.2` is cola's own.
        let observed = "cola/0.164.0 (Arch Linux Unknown; x86_64) WindowsTerminal (cola; 0.14.2)";
        let version = ServerVersion::parse(observed).expect("the observed UA parses");
        assert_eq!(version.to_string(), "0.164.0");
        assert!(!version.is_pinned());

        // A `codex-*` name is preferred even when a non-codex token comes
        // first: the TUI-originated shape keeps its old meaning.
        let preferred = ServerVersion::parse(
            "something/0.150.0 codex-tui/0.164.0 (Arch Linux Unknown; x86_64) WindowsTerminal (cola; 0.0.1)",
        )
        .expect("the codex token wins");
        assert_eq!(preferred.to_string(), "0.164.0");

        let tui = ServerVersion::parse(
            "codex-tui/0.164.0 (Arch Linux Unknown; x86_64) WindowsTerminal (cola; 0.0.1)",
        )
        .expect("version");
        assert_eq!(tui.to_string(), "0.164.0");
        assert!(!tui.is_pinned());

        // The space-separated CLI shape still parses, and is pinned.
        let cli = ServerVersion::parse("codex-cli 0.163.0").expect("cli");
        assert_eq!(cli.to_string(), "0.163.0");
        assert!(cli.is_pinned());

        assert!(ServerVersion::parse("codex-tui/not-a-version").is_none());
        assert!(ServerVersion::parse("cola/not-a-version (cola; 0.14.2)").is_none());
        assert!(ServerVersion::parse("no version here").is_none());
    }

    /// The observed UA attaches end to end: a fixed probe verifies the newer
    /// surface, so construction records 0.164.0 instead of failing at version
    /// parse on the non-`codex` client name.
    #[tokio::test]
    async fn the_observed_live_user_agent_attaches_at_its_server_version() {
        let observed = "cola/0.164.0 (Arch Linux Unknown; x86_64) WindowsTerminal (cola; 0.14.2)";
        let client = connect(observed, &FixedProbe(Surface::pinned()))
            .await
            .expect("the observed UA attaches");
        assert_eq!(client.server_info().version.to_string(), "0.164.0");
        assert_eq!(client.server_info().user_agent, observed);
        assert_eq!(client.server_info().surface_source, "test fixture");
    }

    /// The acceptance criterion: a server that lacks a required method fails at
    /// construction with the method named.
    #[tokio::test]
    async fn a_missing_required_method_fails_at_construction_naming_it() {
        let mut surface = Surface::pinned();
        surface.client_requests.remove("turn/steer");
        // Rename/delete are contract-critical too: administered calls that
        // would otherwise fail on the first `/name` or probe cleanup.
        surface.client_requests.remove("thread/delete");
        surface.client_requests.remove("thread/name/set");
        let error = connect("codex-tui/0.163.0 (test)", &FixedProbe(surface))
            .await
            .expect_err("construction must fail");
        let message = error.to_string();
        assert!(message.contains("turn/steer"), "{message}");
        assert!(message.contains("thread/delete"), "{message}");
        assert!(message.contains("thread/name/set"), "{message}");
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

    /// Ticket #883 live finding: the agent-question feature is a daemon-side
    /// config row (`default_mode_request_user_input`), not a client capability.
    /// The read is best-effort and reflects the server's row each time.
    #[tokio::test]
    async fn the_agent_question_feature_state_comes_from_the_server() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        let endpoint = Endpoint::parse(&url).expect("endpoint");
        let client = CodexClient::connect(&CodexOptions::new(endpoint), &PinnedProbe)
            .await
            .expect("attach to the scripted app-server");
        let row = |enabled: bool| {
            json!({
                "name": AGENT_QUESTIONS_FEATURE,
                "enabled": enabled,
                "defaultEnabled": false,
                "stage": "underDevelopment",
            })
        };

        server.set_experimental_features(vec![row(true)]);
        assert_eq!(client.agent_questions_enabled().await.expect("read"), Some(true));

        server.set_experimental_features(vec![row(false)]);
        assert_eq!(client.agent_questions_enabled().await.expect("read"), Some(false));

        server.set_experimental_features(vec![json!({
            "name": "some_other_feature",
            "enabled": true,
            "defaultEnabled": false,
            "stage": "beta",
        })]);
        assert_eq!(
            client.agent_questions_enabled().await.expect("read"),
            None,
            "an unlisted feature is unknown, not off"
        );
        assert_eq!(
            server.experimental_feature_calls(),
            3,
            "the adapter asks the server rather than caching a guess"
        );
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
        match connect_against_generated_surface_missing("turn/steer").await {
            CodexError::MissingContract { missing, .. } => {
                assert!(missing.contains("turn/steer"), "{missing}");
            }
            other => panic!("expected a missing contract, got {other}"),
        }
    }

    /// Ticket #882 acceptance: a `0.164.0` daemon's generated surface (106
    /// client methods) carries the session-decision set its own response
    /// schemas declare, so the approval card offers the session control. The
    /// same method with the variant removed from its response schema hides it —
    /// the set is the server's answer, never a fixed table.
    #[tokio::test]
    async fn a_generated_surface_decides_the_session_control_its_responses_declare() {
        use crate::codex::schema::{write_generated_surface, write_response_schema};

        let pinned = Surface::pinned();
        let mut client_methods: Vec<&str> = pinned.client_requests.iter().map(String::as_str).collect();
        client_methods.push("feedback/save");
        client_methods.sort_unstable();
        let notifications: Vec<&str> = pinned.notifications.iter().map(String::as_str).collect();
        let server_requests: Vec<&str> = pinned.server_requests.iter().map(String::as_str).collect();
        let modern_files = [
            (
                "CommandExecutionRequestApprovalResponse.json",
                &["accept", "acceptForSession", "decline"][..],
            ),
            (
                "FileChangeRequestApprovalResponse.json",
                &["accept", "acceptForSession", "decline"][..],
            ),
            (
                "ExecCommandApprovalResponse.json",
                &["approved", "approved_for_session", "denied"][..],
            ),
            (
                "ApplyPatchApprovalResponse.json",
                &["approved", "approved_for_session", "denied"][..],
            ),
        ];

        let directory = tempfile::tempdir().expect("tempdir");
        write_generated_surface(
            directory.path(),
            &client_methods,
            &notifications,
            &server_requests,
        )
        .expect("write schema fixture");
        for (file, decisions) in modern_files {
            write_response_schema(directory.path(), file, decisions);
        }
        let surface = Surface::from_generated_dir(directory.path()).expect("generated surface");
        assert_eq!(surface.counts(), (106, 85, 10), "the 0.164.0 method lists");
        assert_eq!(
            surface.session_decisions(),
            Surface::pinned().session_decisions(),
            "the modern response schemas advertise the same four methods"
        );

        let approval = json!({
            "threadId": "thread-1",
            "turnId": "turn-1",
            "itemId": "item-1",
            "command": "ls",
            "cwd": "/work",
        });
        let (_server, endpoint) = server("codex-tui/0.164.0 (test)").await;
        let client = CodexClient::connect(&CodexOptions::new(endpoint), &FixedProbe(surface))
            .await
            .expect("a verified 0.164.0 surface attaches");
        assert_eq!(client.server_info().surface_counts, (106, 85, 10));
        _server.request_to_all("item/commandExecution/requestApproval", approval.clone());
        let permission = recorded_permission(&client, "/work").await;
        assert!(
            permission.always_supported,
            "the card offers the session control the response schema declares"
        );

        // The hypothetical newer daemon: the method is still served, but its
        // response schema no longer carries the variant.
        write_response_schema(
            directory.path(),
            "CommandExecutionRequestApprovalResponse.json",
            &["accept", "decline"],
        );
        let narrowed = Surface::from_generated_dir(directory.path()).expect("generated surface");
        assert!(
            !narrowed
                .session_decisions()
                .contains("item/commandExecution/requestApproval")
        );
        let (_server, endpoint) = server("codex-tui/0.164.0 (test)").await;
        let client = CodexClient::connect(&CodexOptions::new(endpoint), &FixedProbe(narrowed))
            .await
            .expect("the narrowed surface still attaches");
        _server.request_to_all("item/commandExecution/requestApproval", approval);
        let permission = recorded_permission(&client, "/work").await;
        assert!(
            !permission.always_supported,
            "no session control is offered for a variant the server dropped"
        );
    }

    /// Wait for a scripted server approval to be recorded and return it.
    async fn recorded_permission(
        client: &CodexClient,
        directory: &str,
    ) -> crate::backend::types::PermissionRequest {
        use crate::backend::Backend;

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            if let Some(permission) = client
                .list_permissions(Some(directory))
                .await
                .expect("the pending list")
                .into_iter()
                .next()
            {
                return permission;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the approval was never recorded"
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }

    /// A generated surface without `thread/delete` is refused at construction,
    /// naming it: the adapter deletes (rename, and the doctor probe's cleanup)
    /// must never pass the gate and only fail at the first delete.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_generated_surface_missing_thread_delete_fails_at_construction() {
        match connect_against_generated_surface_missing("thread/delete").await {
            CodexError::MissingContract { missing, .. } => {
                assert!(missing.contains("thread/delete"), "{missing}");
            }
            other => panic!("expected a missing contract, got {other}"),
        }
    }

    /// Connect to a scripted newer daemon whose generated schema omits the
    /// client-request method `missing`, and return the construction failure.
    #[cfg(unix)]
    async fn connect_against_generated_surface_missing(missing: &str) -> CodexError {
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
            .filter(|method| *method != missing)
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
        CodexClient::connect(&CodexOptions::new(endpoint), &AutoProbe::new(&binary))
            .await
            .expect_err("a missing generated method fails construction")
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
