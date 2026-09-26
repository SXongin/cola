//! The generation-parameterized `Backend` conformance suite (spec #364,
//! "Testing Decisions"): one neutral scenario set driven through the real
//! adapter — its HTTP transport, the concrete [`OpenCodeBackend`], the selected
//! generation strategy — against each generation's fake payloads. The scenarios
//! assert only what the Bridge consumes; generation-distinctive wire details
//! stay in that generation's own wire suite.
//!
//! Each generation contributes a [`SessionReadCase`] from its own module
//! (`v1::conformance` / `v2::conformance`), so a V1 route literal never leaves
//! the V1 strategy (the coupling guard, spec #364 §1) and V1 retirement deletes
//! its case with the rest of the strategy. The payload *values* are shared via
//! [`SessionReadFixture`], so the two generations' fixtures cannot drift —
//! only the wire spellings around them differ.

use crate::opencode::client::OpenCodeBackend;
use crate::opencode::strategy::Generation;
use crate::test_http::TestHttpServer;

/// One generation's session-read conformance case: which strategy to speak, and
/// how to mount that generation's fake routes.
pub(crate) struct SessionReadCase {
    pub(crate) generation: Generation,
    /// Mount the generation's fake session-read routes (a two-page list, one
    /// session get, the run-state reads), publishing the shared
    /// [`SessionReadFixture`] values in that generation's envelope shapes.
    pub(crate) mount: fn(&TestHttpServer, &SessionReadFixture),
}

/// The neutral values both generations' session-read payloads publish. One
/// shared default keeps the per-generation mounts from carrying duplicate
/// literals that could drift; only the wire spellings around these values
/// differ, and they stay in the generation modules.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SessionReadFixture {
    pub(crate) newest: &'static str,
    pub(crate) child: &'static str,
    pub(crate) oldest: &'static str,
    pub(crate) parent: &'static str,
    pub(crate) title: &'static str,
    pub(crate) child_title: &'static str,
    pub(crate) other_title: &'static str,
    pub(crate) directory: &'static str,
    pub(crate) other_directory: &'static str,
    pub(crate) agent: &'static str,
    pub(crate) provider: &'static str,
    pub(crate) model: &'static str,
    pub(crate) idle: &'static str,
    pub(crate) busy: &'static str,
    pub(crate) retrying: &'static str,
}

impl Default for SessionReadFixture {
    fn default() -> Self {
        Self {
            newest: "ses_new",
            child: "ses_child",
            oldest: "ses_old",
            parent: "ses_parent",
            title: "新",
            child_title: "子会话",
            other_title: "旧",
            directory: "/work/cola",
            other_directory: "/work/other",
            agent: "build",
            provider: "opencode-go",
            model: "deepseek-v4-flash",
            idle: "ses_idle",
            busy: "ses_busy",
            retrying: "ses_retry",
        }
    }
}

impl SessionReadCase {
    /// The real adapter, pointed at the fake server and speaking this case's
    /// generation — production's construction with only the transport swapped
    /// for the no-proxy test one (ADR-0031).
    fn backend(&self, server: &TestHttpServer) -> OpenCodeBackend {
        let backend = OpenCodeBackend::with_generation(
            None,
            server.base_url(),
            Some("opencode"),
            Some("secret"),
            self.generation,
            None,
        );
        backend.disable_env_proxy(Some("opencode"), Some("secret"));
        backend
    }
}

/// Both generations under test. The suite grows a case per generation, never a
/// scenario per generation.
fn cases() -> [SessionReadCase; 2] {
    [
        crate::opencode::v1::conformance::session_read_case(),
        crate::opencode::v2::conformance::session_read_case(),
    ]
}

/// The session list has the same neutral outcome on both generations: every
/// page merged in server order, the directory exposed, and the parent chain the
/// `/switch`/`/sub` surfaces filter on.
#[tokio::test]
async fn list_sessions_yields_the_same_neutral_view_on_every_generation() {
    for case in cases() {
        let generation = case.generation.as_str();
        let fixture = SessionReadFixture::default();
        let server = TestHttpServer::start().await;
        (case.mount)(&server, &fixture);
        let backend = case.backend(&server);

        let sessions = backend
            .list_sessions()
            .await
            .unwrap_or_else(|e| panic!("{generation}: list_sessions failed: {e}"));

        let listed: Vec<&str> = sessions.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(
            listed,
            [fixture.newest, fixture.child, fixture.oldest],
            "{generation}: pages merged in order"
        );
        assert_eq!(
            sessions[0].directory, fixture.directory,
            "{generation}: directory"
        );
        assert_eq!(sessions[0].title, fixture.title, "{generation}: title");
        assert_eq!(
            sessions[0].agent.as_deref(),
            Some(fixture.agent),
            "{generation}: agent"
        );
        assert!(
            sessions[0].parent_id.is_none(),
            "{generation}: a root has no parent"
        );
        assert!(
            sessions[1].is_child(),
            "{generation}: the child keeps its parentID"
        );
        assert!(
            sessions[1].is_child_of(fixture.parent),
            "{generation}: parentID value"
        );
        assert_eq!(
            sessions[1].title, fixture.child_title,
            "{generation}: child title"
        );
        assert_eq!(
            sessions[2].directory, fixture.other_directory,
            "{generation}: a session in another directory keeps its own"
        );
        assert_eq!(
            sessions[2].title, fixture.other_title,
            "{generation}: the oldest row's title"
        );
        assert_eq!(
            sessions[0]
                .model
                .as_ref()
                .and_then(|m| m.get("providerID"))
                .and_then(|p| p.as_str()),
            Some(fixture.provider),
            "{generation}: the row's model survives for display"
        );
    }
}

/// The session get has the same neutral outcome on both generations: the
/// parent chain and the server-recorded model, which the effective-model
/// ladder and sub-task re-homing read.
#[tokio::test]
async fn session_info_exposes_parent_and_model_on_every_generation() {
    for case in cases() {
        let generation = case.generation.as_str();
        let fixture = SessionReadFixture::default();
        let server = TestHttpServer::start().await;
        (case.mount)(&server, &fixture);
        let backend = case.backend(&server);

        let info = backend
            .session_info(fixture.child, Some(fixture.directory))
            .await
            .unwrap_or_else(|e| panic!("{generation}: session_info failed: {e}"));

        assert_eq!(info.id, fixture.child, "{generation}: id");
        assert_eq!(
            info.parent_id.as_deref(),
            Some(fixture.parent),
            "{generation}: parent chain"
        );
        assert_eq!(
            info.title.as_deref(),
            Some(fixture.child_title),
            "{generation}: title"
        );
        let model = info.model.expect("the server-recorded model must parse");
        assert_eq!(
            model.provider_id, fixture.provider,
            "{generation}: model provider"
        );
        assert_eq!(model.id, fixture.model, "{generation}: model id");
    }
}

/// The run state has the same neutral outcome on both generations: Idle, Busy
/// and Retry. V2 derives Retry from the newest assistant message's `retry`
/// field where V1 reads it from the status map, but the Session Snapshot's
/// status line cannot tell them apart.
#[tokio::test]
async fn session_status_maps_idle_busy_retry_on_every_generation() {
    use crate::opencode::types::SessionStatus;

    for case in cases() {
        let generation = case.generation.as_str();
        let fixture = SessionReadFixture::default();
        let server = TestHttpServer::start().await;
        (case.mount)(&server, &fixture);
        let backend = case.backend(&server);

        for (session_id, expected, label) in [
            (fixture.idle, Some(SessionStatus::Idle), "idle"),
            (fixture.busy, Some(SessionStatus::Busy), "busy"),
            (fixture.retrying, Some(SessionStatus::Retry), "retry"),
        ] {
            let status = backend
                .session_status(session_id, Some(fixture.directory))
                .await
                .unwrap_or_else(|e| panic!("{generation}/{label}: session_status failed: {e}"));
            assert_eq!(status, expected, "{generation}/{label}: run state");
        }
    }
}
