//! Adapter-level wire tests: the `OpenCodeBackend` calls that are not owned by
//! a generation strategy — session creation and compaction, on the `/api`
//! surface both generations serve. They drive the real backend against a local
//! fake server and assert both sides of the exchange (ADR-0031).

use super::client::OpenCodeBackend;
use super::parsing::parse_model;
use super::types::{CreateSessionInput, Location};
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
        Some(crate::bridge::discovery::ResolvedServer {
            url: "http://localhost:4096".to_string(),
            username: "opencode".to_string(),
            password: "secret".to_string(),
            pid: Some(4242),
            generation: super::strategy::Generation::V1,
        }),
    );
    assert_eq!(backend.attached_server_pid(), Some(4242));

    backend.reconnect(Some(&crate::bridge::discovery::ResolvedServer {
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

#[tokio::test]
async fn compact_posts_the_current_generation_route() {
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
