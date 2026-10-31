//! Adapter-level wire tests: session creation, the one call that lives on the
//! generation-blind `OpenCodeBackend` itself (`/api/session`, served by both
//! generations), plus V1's strategy-owned compaction shape driven through the
//! adapter. They drive the real backend against a local fake server and assert
//! both sides of the exchange (ADR-0031).

use super::client::OpenCodeBackend;
use crate::backend::parsing::parse_model;
use crate::backend::types::{CreateSessionInput, Location};
use crate::test_http::{RecordedRequest, TestHttpServer};

/// A backend pointed at the fake server with both Basic-auth parts set — what
/// discovery hands production. The transport is swapped for a no-proxy one so
/// a developer shell's `http_proxy` cannot intercept the loopback fake server
/// (ticket 09); construction itself stays production's.
fn wire_backend(server: &TestHttpServer) -> OpenCodeBackend {
    let backend = OpenCodeBackend::with_base_url(None, server.base_url(), Some("opencode"), Some("secret"));
    backend.disable_env_proxy(Some("opencode"), Some("secret"));
    backend
}

/// Issue #908: a `/provider` request the server accepts and then stalls is
/// bounded. A callback read returns **inside its budget** with the degraded
/// surface (known content plus a note), so a detached card handler cannot
/// outlive Feishu's ack deadline.
#[tokio::test]
async fn a_stalled_provider_read_returns_inside_the_callback_budget() {
    let server = TestHttpServer::start().await;
    server.route_delayed(
        "GET",
        crate::opencode::v1::PROVIDER_ROUTE_FOR_TESTS,
        200,
        "{}",
        std::time::Duration::from_secs(30),
    );
    let backend = wire_backend(&server);
    let started = std::time::Instant::now();
    let surface = backend
        .model_surface(crate::backend::types::ModelSurfaceQuery {
            ack_budget: Some(crate::feishu::ws::CARD_ACK_BUDGET),
            ..Default::default()
        })
        .await;
    let elapsed = started.elapsed();
    assert!(
        elapsed < crate::feishu::ws::CARD_ACK_BUDGET,
        "the stalled read is bounded by the callback budget: {elapsed:?}"
    );
    let note = surface
        .scopes
        .first()
        .and_then(|scope| scope.note.as_deref())
        .unwrap_or_default();
    assert!(
        note.contains("超时"),
        "the degradation is stated instead of hanging: {note}"
    );
}

fn last_request(server: &TestHttpServer) -> RecordedRequest {
    server.requests().pop().expect("a request should have been sent")
}

fn body_json(request: &RecordedRequest) -> serde_json::Value {
    serde_json::from_str(&request.body).expect("request body should be JSON")
}

/// The reconnect loop notices a same-URL replacement by comparing the
/// attachment's pid (spec #364 §2): the adapter must carry it from the
/// resolved server and clear it when it goes serverless.
#[test]
fn reconnect_tracks_the_attached_server_pid() {
    let backend = OpenCodeBackend::new(
        None,
        Some(crate::opencode::discovery::ResolvedServer {
            url: "http://localhost:4096".to_string(),
            username: "opencode".to_string(),
            password: "secret".to_string(),
            pid: Some(4242),
            generation: super::strategy::Generation::V1,
        }),
    );
    assert_eq!(backend.attached_server_pid(), Some(4242));

    backend.reconnect(Some(&crate::opencode::discovery::ResolvedServer {
        url: "http://localhost:49374".to_string(),
        username: "opencode".to_string(),
        password: "new-secret".to_string(),
        pid: Some(5252),
        generation: super::strategy::Generation::V2,
    }));
    assert_eq!(backend.attached_server_pid(), Some(5252));

    backend.reconnect(None);
    assert_eq!(backend.attached_server_pid(), None);
    assert_eq!(backend.base_url(), "");
}

#[test]
fn a_serverless_backend_has_no_attached_pid() {
    let backend = OpenCodeBackend::new(None, None);
    assert_eq!(backend.attached_server_pid(), None);
}

#[tokio::test]
async fn create_session_posts_the_input_and_parses_the_data_envelope() {
    let server = TestHttpServer::start().await;
    server.route(
        "POST",
        "/api/session",
        200,
        serde_json::json!({
            "data": {
                "id": "ses_new",
                "projectID": "proj_x",
                "agent": "build",
                "cost": 0.0,
                "time": {"created": 1700000000000i64, "updated": 1700000100000i64},
                "title": "新会话",
                "location": {"directory": "/work/cola"},
            },
        })
        .to_string(),
    );
    let backend = wire_backend(&server);
    let input = CreateSessionInput {
        id: None,
        agent: Some("build".to_string()),
        model: parse_model("opencode-go/deepseek-v4-flash"),
        location: Some(Location {
            directory: "/work/cola".to_string(),
        }),
    };

    let session = backend.create_session(&input).await.unwrap();

    assert_eq!(session.id, "ses_new");
    assert_eq!(session.project_id.as_deref(), Some("proj_x"));
    assert_eq!(session.title.as_deref(), Some("新会话"));
    assert_eq!(session.agent.as_deref(), Some("build"));
    assert_eq!(
        session.time.as_ref().map(|time| time.created),
        Some(1700000000000)
    );

    let request = last_request(&server);
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, "/api/session");
    assert_eq!(request.query, "");
    let body = body_json(&request);
    assert!(body.get("id").is_none(), "an unset id must be omitted: {body}");
    assert_eq!(body["agent"], "build");
    assert_eq!(body["model"]["providerID"], "opencode-go");
    assert_eq!(body["model"]["id"], "deepseek-v4-flash");
    assert_eq!(body["location"]["directory"], "/work/cola");
}

/// V1's compaction shape, driven through the generation-blind adapter: the
/// shared `/api/session/{id}/compact` route with no request body (V2's
/// strategy sends its own empty payload).
#[tokio::test]
async fn v1_compact_posts_the_shared_route_without_a_body() {
    let server = TestHttpServer::start().await;
    server.route("POST", "/api/session/ses_1/compact", 200, r#"{"data":{}}"#);
    let backend = wire_backend(&server);

    backend.compact("ses_1").await.unwrap();

    let request = last_request(&server);
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, "/api/session/ses_1/compact");
    assert_eq!(request.query, "");
    assert_eq!(request.body, "", "compact carries no body");
}

/// The `/model` surface's root page is OpenCode's provider list: one
/// `provider` scope, the provider ids as callback values, and the caller's
/// current selection on top — the same shape the hand-built card had.
#[test]
fn model_surface_root_page_lists_providers_and_keeps_the_current_line() {
    use super::client::model_surface_from_providers;
    use crate::backend::types::{ModelOption, ModelSelection, ModelSurfaceQuery, ProviderModels};

    let providers = vec![
        ProviderModels {
            provider: "opencode".into(),
            models: vec![ModelOption::new("deepseek-v4-flash")],
        },
        ProviderModels {
            provider: "openrouter".into(),
            models: vec![ModelOption::new("gpt-4o")],
        },
    ];
    let query = ModelSurfaceQuery {
        session_id: Some("ses_1"),
        directory: Some("/work/cola"),
        current: Some(ModelSelection {
            label: "opencode/deepseek-v4-flash@low".into(),
            reference: "opencode/deepseek-v4-flash".into(),
        }),
        ..Default::default()
    };

    let surface = model_surface_from_providers(&providers, &query);

    assert_eq!(surface.scopes.len(), 1);
    let scope = &surface.scopes[0];
    assert_eq!(scope.id, "provider");
    assert_eq!(scope.title, "选择模型");
    assert_eq!(
        scope.options.iter().map(|o| o.id.as_str()).collect::<Vec<_>>(),
        ["opencode", "openrouter"]
    );
    assert_eq!(
        scope.options[0].callback_value(),
        "opencode",
        "the provider button's value is the provider id"
    );
    assert!(
        scope.intro.as_deref().unwrap().contains("共 2 个"),
        "provider count: {:?}",
        scope.intro
    );
    assert_eq!(
        surface.current.as_ref().map(|c| c.label.as_str()),
        Some("opencode/deepseek-v4-flash@low"),
        "the current line is the caller's resolved selection"
    );
}

/// A drilled page is that provider's model list: bare model-id labels,
/// `provider/model` callback values, the declared variants, and no current
/// line (the model page never showed one).
#[test]
fn model_surface_provider_page_keeps_values_and_variants() {
    use super::client::model_surface_from_providers;
    use crate::backend::types::{ModelOption, ModelSelection, ModelSurfaceQuery, ProviderModels};

    let providers = vec![ProviderModels {
        provider: "opencode-go".into(),
        models: vec![
            ModelOption {
                id: "deepseek-v4-pro".into(),
                variants: vec!["low".into(), "high".into()],
                ..Default::default()
            },
            ModelOption::new("deepseek-v4-flash"),
        ],
    }];
    let query = ModelSurfaceQuery {
        current: Some(ModelSelection {
            label: "opencode-go/deepseek-v4-pro@high".into(),
            reference: "opencode-go/deepseek-v4-pro".into(),
        }),
        scope: Some("provider"),
        selected: Some("opencode-go"),
        ..Default::default()
    };

    let surface = model_surface_from_providers(&providers, &query);

    let scope = &surface.scopes[0];
    assert_eq!(scope.id, "model");
    assert_eq!(scope.title, "opencode-go", "the provider heads the page");
    assert_eq!(
        scope
            .options
            .iter()
            .map(|o| o.display_label())
            .collect::<Vec<_>>(),
        ["deepseek-v4-pro", "deepseek-v4-flash"],
        "the label stays the bare model id"
    );
    assert_eq!(
        scope
            .options
            .iter()
            .map(|o| o.callback_value())
            .collect::<Vec<_>>(),
        ["opencode-go/deepseek-v4-pro", "opencode-go/deepseek-v4-flash"]
    );
    assert_eq!(scope.options[0].variants, ["low", "high"]);
    assert!(
        surface.current.is_none(),
        "the model page omitted the current line before, so it still does"
    );
}

/// No providers degrades to the same single "no models" page; a drilled page
/// for a provider that no longer exists keeps the old empty-model-page shape
/// (the stale callback cannot strand the user on a missing page).
#[test]
fn model_surface_degrades_to_the_empty_root_page() {
    use super::client::model_surface_from_providers;
    use crate::backend::types::{ModelSelection, ModelSurfaceQuery};

    let query = ModelSurfaceQuery {
        current: Some(ModelSelection {
            label: "opencode-go/deepseek-v4-flash@low".into(),
            reference: "opencode-go/deepseek-v4-flash".into(),
        }),
        ..Default::default()
    };
    let surface = model_surface_from_providers(&[], &query);
    let scope = &surface.scopes[0];
    assert_eq!(scope.id, "provider");
    assert_eq!(scope.intro.as_deref(), Some("_(没有可用模型)_"));
    assert_eq!(
        surface.current.as_ref().map(|c| c.label.as_str()),
        Some("opencode-go/deepseek-v4-flash@low")
    );

    let drilled = ModelSurfaceQuery {
        scope: Some("provider"),
        selected: Some("gone"),
        ..Default::default()
    };
    let surface = model_surface_from_providers(
        &[crate::backend::types::ProviderModels {
            provider: "opencode".into(),
            models: Vec::new(),
        }],
        &drilled,
    );
    assert_eq!(surface.scopes[0].id, "model");
    assert_eq!(
        surface.scopes[0].intro.as_deref(),
        Some("_(该 provider 没有可用模型)_")
    );
}
