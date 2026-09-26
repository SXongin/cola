//! The V1 suite: payload-shape tests over the strategy's private builders, and
//! wire tests driving the real adapter (which defaults to the V1 strategy)
//! against a local fake server, asserting both sides of every exchange — the
//! recorded request (method / path / query / headers / body) and what the
//! client parsed back (ADR-0031). These pin the V1 wire contract; V2 grows the
//! same suite shape beside its strategy.

use super::*;
use crate::error::BridgeError;
use crate::opencode::client::OpenCodeBackend;
use crate::opencode::parsing::parse_model;
use crate::test_http::{MockResponse, RecordedRequest, TestHttpServer};
use base64::Engine;

#[test]
fn inject_model_writes_variant_independently_of_model() {
    // A variant is sent even when NO model is set (the server applies it to
    // whatever model runs this turn); when a model IS set, both are written.
    let mut body = serde_json::json!({ "parts": [] });
    inject_model(&mut body, None, Some("high"));
    assert_eq!(body["variant"], "high");
    assert!(body.get("model").is_none(), "no model, only variant: {body}");

    let mut body2 = serde_json::json!({ "parts": [] });
    let model = parse_model("opencode-go/deepseek-v4-flash").unwrap();
    inject_model(&mut body2, Some(&model), Some("high"));
    assert_eq!(body2["model"]["modelID"], "deepseek-v4-flash");
    assert_eq!(body2["variant"], "high");

    // Unset variant leaves the body free of the field.
    let mut body3 = serde_json::json!({ "parts": [] });
    inject_model(&mut body3, None, None);
    assert!(body3.get("variant").is_none());
}

#[test]
fn parses_provider_models_with_object_variants() {
    // Real `GET /provider` shape (captured): `model.variants` is a Record
    // keyed by variant id (`{"low": {...}, "high": {...}, "max": {...}}`),
    // NOT an array of `{id}` objects. Only `connected` providers surface.
    let json = serde_json::json!({
        "all": [
            {
                "id": "opencode-go",
                "name": "OpenCode Go",
                "models": {
                    "deepseek-v4-flash": {
                        "id": "deepseek-v4-flash",
                        "providerID": "opencode-go",
                        "name": "DeepSeek v4 Flash",
                        "variants": {
                            "low": { "reasoningEffort": "low" },
                            "high": { "reasoningEffort": "high" },
                            "max": { "reasoningEffort": "max" }
                        }
                    },
                    "deepseek-v4-pro": {
                        "id": "deepseek-v4-pro",
                        "name": "DeepSeek v4 Pro"
                    }
                }
            },
            {
                "id": "not-connected-provider",
                "models": { "x": { "id": "x", "name": "X" } }
            }
        ],
        "connected": ["opencode-go"]
    });
    let models = parse_provider_models(&json);
    assert_eq!(models.len(), 1, "only connected providers surface: {models:?}");
    assert_eq!(models[0].provider, "opencode-go");
    let flash = models[0]
        .models
        .iter()
        .find(|m| m.id == "deepseek-v4-flash")
        .unwrap();
    assert_eq!(flash.variants, vec!["high", "low", "max"]);
    let pro = models[0]
        .models
        .iter()
        .find(|m| m.id == "deepseek-v4-pro")
        .unwrap();
    assert!(pro.variants.is_empty(), "a model with no variants stays empty");
}

#[test]
fn parses_provider_models_with_legacy_array_variants() {
    // Very old servers sent `variants` as an array of `{id, ...}` objects;
    // both shapes must resolve to the same variant names.
    let json = serde_json::json!({
        "all": [
            {
                "id": "p",
                "models": {
                    "m": {
                        "id": "m",
                        "name": "M",
                        "variants": [ { "id": "high", "name": "High" }, { "id": "low", "name": "Low" } ]
                    }
                }
            }
        ]
    });
    let models = parse_provider_models(&json);
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].models[0].variants, vec!["high", "low"]);
}

#[test]
fn parses_provider_models_without_connected_keeps_all() {
    // No `connected` field (older server): every provider is kept.
    let json = serde_json::json!({
        "all": [
            { "id": "a", "models": { "m": { "id": "m", "name": "M" } } },
            { "id": "b", "models": { "n": { "id": "n", "name": "N" } } }
        ]
    });
    let models = parse_provider_models(&json);
    assert_eq!(models.len(), 2);
}

#[test]
fn parses_session_status_entry_types() {
    assert_eq!(
        parse_session_status_entry(&serde_json::json!({"type": "idle"})),
        Some(SessionStatus::Idle)
    );
    assert_eq!(
        parse_session_status_entry(&serde_json::json!({"type": "busy"})),
        Some(SessionStatus::Busy)
    );
    // The retry entry carries extra fields; cola reads only `type`.
    assert_eq!(
        parse_session_status_entry(&serde_json::json!({
            "type": "retry", "attempt": 1, "message": "boom", "next": 5000
        })),
        Some(SessionStatus::Retry)
    );
    // Unknown/absent type is never guessed.
    assert_eq!(
        parse_session_status_entry(&serde_json::json!({"type": "zombie"})),
        None
    );
    assert_eq!(
        parse_session_status_entry(&serde_json::json!({"no": "type"})),
        None
    );
}

/// A client pointed at the fake server with both Basic-auth parts set —
/// what discovery hands production. The transport is swapped for a
/// no-proxy one so a developer shell's `http_proxy` cannot intercept the
/// loopback fake server (ticket 09); construction itself stays production's.
fn v1_wire_client(server: &TestHttpServer, model: Option<&str>) -> OpenCodeBackend {
    without_env_proxy(
        OpenCodeBackend::with_base_url(model, server.base_url(), Some("opencode"), Some("secret")),
        Some("opencode"),
        Some("secret"),
    )
}

/// Point a wire-test client at the no-proxy transport, keeping every other
/// byte of its construction exactly as production built it (ADR-0031). The
/// transport flip is the adapter's test-only hook.
fn without_env_proxy(
    client: OpenCodeBackend,
    username: Option<&str>,
    password: Option<&str>,
) -> OpenCodeBackend {
    client.disable_env_proxy(username, password);
    client
}

/// The request at `index` in arrival order.
fn request_at(server: &TestHttpServer, index: usize) -> RecordedRequest {
    server
        .requests()
        .get(index)
        .cloned()
        .unwrap_or_else(|| panic!("request {index} should have been sent"))
}

fn last_request(server: &TestHttpServer) -> RecordedRequest {
    server.requests().pop().expect("a request should have been sent")
}

fn body_json(request: &RecordedRequest) -> serde_json::Value {
    serde_json::from_str(&request.body).expect("request body should be JSON")
}

fn expected_basic(username: &str, password: &str) -> String {
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"))
    )
}

fn opencode_error(err: BridgeError) -> String {
    match err {
        BridgeError::OpenCode(message) => message,
        other => panic!("expected BridgeError::OpenCode, got: {other:?}"),
    }
}

fn not_found_error(err: BridgeError) -> String {
    match err {
        BridgeError::NotFound(message) => message,
        other => panic!("expected BridgeError::NotFound, got: {other:?}"),
    }
}

#[tokio::test]
async fn new_binds_the_resolved_server_url_and_credentials() {
    let server = TestHttpServer::start().await;
    server.route("GET", "/provider", 200, r#"{"all":[],"connected":[]}"#);
    let client = without_env_proxy(
        OpenCodeBackend::new(
            None,
            Some(crate::bridge::discovery::ResolvedServer {
                url: server.base_url(),
                username: "custom-user".to_string(),
                password: "custom-pass".to_string(),
            }),
        ),
        Some("custom-user"),
        Some("custom-pass"),
    );

    assert_eq!(client.base_url(), server.base_url());
    let _ = client.list_models().await;
    let expected = expected_basic("custom-user", "custom-pass");
    assert_eq!(
        last_request(&server).header("authorization"),
        Some(expected.as_str())
    );
}

#[tokio::test]
async fn with_base_url_trims_a_trailing_slash() {
    let server = TestHttpServer::start().await;
    server.route("GET", "/provider", 200, r#"{"all":[],"connected":[]}"#);
    let client = without_env_proxy(
        OpenCodeBackend::with_base_url(None, format!("{}/", server.base_url()), None, None),
        None,
        None,
    );

    assert_eq!(client.base_url(), server.base_url());
    assert_eq!(client.model_context_window("p", "m").await.unwrap(), None);
    assert_eq!(last_request(&server).path, "/provider");
}

#[tokio::test]
async fn basic_auth_is_sent_only_when_both_credentials_are_present() {
    let server = TestHttpServer::start().await;
    server.route("GET", "/provider", 200, r#"{"all":[],"connected":[]}"#);

    struct Case {
        label: &'static str,
        username: Option<&'static str>,
        password: Option<&'static str>,
        expected_auth: Option<String>,
    }
    let cases = [
        Case {
            label: "both",
            username: Some("opencode"),
            password: Some("secret"),
            expected_auth: Some(expected_basic("opencode", "secret")),
        },
        Case {
            label: "username only",
            username: Some("opencode"),
            password: None,
            expected_auth: None,
        },
        Case {
            label: "password only",
            username: None,
            password: Some("secret"),
            expected_auth: None,
        },
        Case {
            label: "neither",
            username: None,
            password: None,
            expected_auth: None,
        },
    ];
    for case in &cases {
        let client = without_env_proxy(
            OpenCodeBackend::with_base_url(None, server.base_url(), case.username, case.password),
            case.username,
            case.password,
        );
        let _ = client.list_models().await;
    }

    assert_eq!(server.request_count(), cases.len());
    for (index, case) in cases.iter().enumerate() {
        assert_eq!(
            request_at(&server, index).header("authorization"),
            case.expected_auth.as_deref(),
            "case: {}",
            case.label
        );
    }
}

#[tokio::test]
async fn prompt_posts_the_message_endpoint_with_parts_model_variant_agent_and_message_id() {
    let server = TestHttpServer::start().await;
    server.route(
        "POST",
        "/session/ses_1/message",
        200,
        serde_json::json!({
            "info": {"id": "msg_a1", "parentID": "msg_u1"},
            "parts": [{"type": "text", "text": "reply"}],
        })
        .to_string(),
    );
    let client = v1_wire_client(&server, None);
    let model = parse_model("opencode-go/deepseek-v4-flash").unwrap();
    let images = vec![ImageInput {
        mime: "image/png".to_string(),
        data_base64: "QUJD".to_string(),
    }];

    let response = client
        .prompt(
            "ses_1",
            "hello",
            &images,
            Some(&model),
            Some("high"),
            Some("build"),
            Some("msg_cola_abc"),
        )
        .await
        .unwrap();

    assert_eq!(response.id, "msg_a1");
    assert_eq!(response.parent_id.as_deref(), Some("msg_u1"));
    assert_eq!(
        response.parts,
        vec![crate::backend::Part::Text(crate::backend::TextPart {
            text: "reply".into(),
            started_at: None,
        })]
    );
    assert!(response.error.is_none());

    let request = last_request(&server);
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, "/session/ses_1/message");
    assert_eq!(request.query, "");
    assert_eq!(request.header("content-type"), Some("application/json"));
    let expected = expected_basic("opencode", "secret");
    assert_eq!(request.header("authorization"), Some(expected.as_str()));
    let body = body_json(&request);
    assert_eq!(body["parts"][0]["type"], "text");
    assert_eq!(body["parts"][0]["text"], "hello");
    assert_eq!(body["parts"][1]["type"], "file");
    assert_eq!(body["parts"][1]["mime"], "image/png");
    assert_eq!(body["parts"][1]["url"], "data:image/png;base64,QUJD");
    assert_eq!(body["model"]["providerID"], "opencode-go");
    assert_eq!(body["model"]["modelID"], "deepseek-v4-flash");
    assert_eq!(body["variant"], "high");
    assert_eq!(body["agent"], "build");
    assert_eq!(body["messageID"], "msg_cola_abc");
}

/// ADR-0048: a successful prompt's response body is a full payload dump —
/// DEBUG, never INFO. The body still parses; only its level moved.
#[tokio::test]
async fn prompt_response_body_is_dumped_at_debug_not_info() {
    let server = TestHttpServer::start().await;
    server.route(
        "POST",
        "/session/ses_1/message",
        200,
        serde_json::json!({
            "info": {"id": "msg_a1"},
            "parts": [{"type": "text", "text": "response body marker"}],
        })
        .to_string(),
    );
    let client = v1_wire_client(&server, None);
    let (response, logs) = crate::bridge::test_support::capture_logs(async {
        client.prompt("ses_1", "hi", &[], None, None, None, None).await
    })
    .await;
    response.unwrap();

    let dump = crate::bridge::test_support::assert_line_level(&logs, "prompt response:", "DEBUG");
    assert!(
        dump.contains("response body marker"),
        "the dump carries the response body: {dump}"
    );
}

#[tokio::test]
async fn prompt_model_prefers_the_override_then_the_configured_default_then_the_server() {
    let server = TestHttpServer::start().await;
    server.route(
        "POST",
        "/session/ses_override/message",
        200,
        r#"{"info":{"id":"msg_1"},"parts":[]}"#,
    );
    server.route(
        "POST",
        "/session/ses_default/message",
        200,
        r#"{"info":{"id":"msg_2"},"parts":[]}"#,
    );
    server.route(
        "POST",
        "/session/ses_server/message",
        200,
        r#"{"info":{"id":"msg_3"},"parts":[]}"#,
    );
    let client = v1_wire_client(&server, Some("opencode-go/configured-model"));
    let override_model = parse_model("other/override-model").unwrap();

    client
        .prompt("ses_override", "a", &[], Some(&override_model), None, None, None)
        .await
        .unwrap();
    client
        .prompt("ses_default", "b", &[], None, None, None, None)
        .await
        .unwrap();
    let client_without_default = v1_wire_client(&server, None);
    client_without_default
        .prompt("ses_server", "c", &[], None, None, None, None)
        .await
        .unwrap();

    let override_body = body_json(&request_at(&server, 0));
    assert_eq!(override_body["model"]["providerID"], "other");
    assert_eq!(override_body["model"]["modelID"], "override-model");
    let default_body = body_json(&request_at(&server, 1));
    assert_eq!(default_body["model"]["providerID"], "opencode-go");
    assert_eq!(default_body["model"]["modelID"], "configured-model");
    let server_body = body_json(&request_at(&server, 2));
    assert!(
        server_body.get("model").is_none(),
        "neither override nor default must leave the model to the server: {server_body}"
    );
}

#[tokio::test]
async fn prompt_surfaces_a_provider_error_carried_on_a_200_response() {
    let server = TestHttpServer::start().await;
    server.route(
        "POST",
        "/session/ses_1/message",
        200,
        serde_json::json!({
            "info": {"id": "msg_a1", "error": {"data": {"message": "provider 503"}}},
            "parts": [],
        })
        .to_string(),
    );
    let client = v1_wire_client(&server, None);

    let response = client
        .prompt("ses_1", "hi", &[], None, None, None, None)
        .await
        .unwrap();

    assert_eq!(response.error.as_deref(), Some("provider 503"));
}

#[tokio::test]
async fn prompt_maps_404_to_session_not_found() {
    let server = TestHttpServer::start().await; // no route -> 404
    let client = v1_wire_client(&server, None);

    let err = client
        .prompt("ses_gone", "hi", &[], None, None, None, None)
        .await
        .unwrap_err();

    match err {
        BridgeError::SessionNotFound(id) => assert_eq!(id, "ses_gone"),
        other => panic!("expected BridgeError::SessionNotFound, got: {other:?}"),
    }
    assert_eq!(last_request(&server).path, "/session/ses_gone/message");
}

#[tokio::test]
async fn prompt_maps_a_failed_status_to_a_diagnostic_opencode_error() {
    let server = TestHttpServer::start().await;
    server.route("POST", "/session/ses_1/message", 500, r#"{"error":"boom"}"#);
    let client = v1_wire_client(&server, None);

    let message = opencode_error(
        client
            .prompt("ses_1", "hi", &[], None, None, None, None)
            .await
            .unwrap_err(),
    );

    assert!(message.contains("prompt ses_1 failed"), "unexpected: {message}");
    assert!(message.contains("500"), "unexpected: {message}");
    assert!(message.contains("boom"), "unexpected: {message}");
}

#[tokio::test]
async fn prompt_reports_a_non_json_success_body_as_a_decode_error() {
    let server = TestHttpServer::start().await;
    server.route_raw(
        "POST",
        "/session/ses_1/message",
        200,
        "text/html",
        "<html>oops</html>",
    );
    let client = v1_wire_client(&server, None);

    let message = opencode_error(
        client
            .prompt("ses_1", "hi", &[], None, None, None, None)
            .await
            .unwrap_err(),
    );

    assert!(message.contains("prompt decode"), "unexpected: {message}");
    assert!(message.contains("oops"), "unexpected: {message}");
}

#[tokio::test]
async fn prompt_async_posts_fire_and_forget_with_the_same_payload() {
    let server = TestHttpServer::start().await;
    server.route("POST", "/session/ses_1/prompt_async", 204, "");
    let client = v1_wire_client(&server, None);
    let model = parse_model("opencode-go/deepseek-v4-flash").unwrap();

    client
        .prompt_async(
            "ses_1",
            "supplement",
            &[],
            Some(&model),
            Some("low"),
            Some("build"),
            Some("msg_cola_def"),
        )
        .await
        .unwrap();

    let request = last_request(&server);
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, "/session/ses_1/prompt_async");
    assert_eq!(request.query, "");
    let body = body_json(&request);
    assert_eq!(body["parts"][0]["type"], "text");
    assert_eq!(body["parts"][0]["text"], "supplement");
    assert_eq!(body["model"]["modelID"], "deepseek-v4-flash");
    assert_eq!(body["variant"], "low");
    assert_eq!(body["agent"], "build");
    assert_eq!(body["messageID"], "msg_cola_def");
}

#[tokio::test]
async fn prompt_async_maps_a_failed_status_to_a_diagnostic_opencode_error() {
    let server = TestHttpServer::start().await;
    server.route("POST", "/session/ses_1/prompt_async", 500, "nope");
    let client = v1_wire_client(&server, None);

    let message = opencode_error(
        client
            .prompt_async("ses_1", "hi", &[], None, None, None, None)
            .await
            .unwrap_err(),
    );

    assert!(message.contains("prompt_async ses_1"), "unexpected: {message}");
    assert!(message.contains("500"), "unexpected: {message}");
    assert!(message.contains("nope"), "unexpected: {message}");
}

#[tokio::test]
async fn prompt_async_maps_404_to_session_not_found() {
    let server = TestHttpServer::start().await; // no route -> 404
    let client = v1_wire_client(&server, None);

    let err = client
        .prompt_async("ses_gone", "hi", &[], None, None, None, None)
        .await
        .unwrap_err();

    match err {
        BridgeError::SessionNotFound(id) => assert_eq!(id, "ses_gone"),
        other => panic!("expected BridgeError::SessionNotFound, got: {other:?}"),
    }
}

#[tokio::test]
async fn list_sessions_gets_the_experimental_route_and_parses_camelcase_entries() {
    let server = TestHttpServer::start().await;
    server.route(
        "GET",
        "/experimental/session",
        200,
        serde_json::json!([{
            "id": "ses_a",
            "title": "重构",
            "directory": "/work/cola",
            "parentID": "ses_parent",
            "agent": "build",
            "model": {"id": "deepseek-v4-flash", "providerID": "opencode-go"},
            "time": {"created": 1700000000000i64, "updated": 1700000100000i64},
        }])
        .to_string(),
    );
    let client = v1_wire_client(&server, None);

    let sessions = client.list_sessions().await.unwrap();

    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].id, "ses_a");
    assert_eq!(sessions[0].title, "重构");
    assert_eq!(sessions[0].directory, "/work/cola");
    assert_eq!(sessions[0].parent_id.as_deref(), Some("ses_parent"));
    assert!(sessions[0].is_child());

    let request = last_request(&server);
    assert_eq!(request.method, "GET");
    assert_eq!(request.path, "/experimental/session");
    assert_eq!(
        request.query, "",
        "no roots filter: children stay in the set for the caller's child policy"
    );
    assert_eq!(server.request_count(), 1, "one page, no cursor to follow");
}

/// Issue #325: the server applies its page limit before cola's client-side
/// child filter, so the listing follows `x-next-cursor` to the end and
/// merges every page.
#[tokio::test]
async fn list_sessions_follows_the_cursor_to_the_end() {
    let server = TestHttpServer::start().await;
    server.route_sequence(
        "GET",
        "/experimental/session",
        vec![
            MockResponse::json(
                serde_json::json!([{
                    "id": "ses_new",
                    "title": "新",
                    "directory": "/work/cola",
                    "time": {"created": 1700000000000i64, "updated": 1700000200000i64},
                }])
                .to_string(),
            )
            .header("x-next-cursor", "1700000200000"),
            MockResponse::json(
                serde_json::json!([{
                    "id": "ses_old",
                    "title": "旧",
                    "directory": "/work/other",
                    "time": {"created": 1700000000000i64, "updated": 1700000100000i64},
                }])
                .to_string(),
            ),
        ],
    );
    let client = v1_wire_client(&server, None);

    let sessions = client.list_sessions().await.unwrap();

    let ids: Vec<&str> = sessions.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(ids, ["ses_new", "ses_old"], "both pages merged in order");
    assert_eq!(server.request_count(), 2, "the cursor was followed once");
    let first = request_at(&server, 0);
    assert_eq!(first.query, "", "the first page carries no cursor");
    let second = request_at(&server, 1);
    assert_eq!(
        second.query_param("cursor").as_deref(),
        Some("1700000200000"),
        "the second page asks for rows older than the first's cutoff"
    );
    assert_eq!(
        second.query_param("roots"),
        None,
        "no roots filter on follow-up pages either"
    );
}

#[tokio::test]
async fn list_sessions_falls_back_to_the_project_scoped_route_on_404() {
    let server = TestHttpServer::start().await;
    server.route("GET", "/experimental/session", 404, r#"{"error":"no"}"#);
    server.route(
        "GET",
        "/session",
        200,
        serde_json::json!([{
            "id": "ses_old",
            "title": "old",
            "directory": "/w",
            "time": {"created": 1, "updated": 2},
        }])
        .to_string(),
    );
    let client = v1_wire_client(&server, None);

    let sessions = client.list_sessions().await.unwrap();

    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].id, "ses_old");
    assert_eq!(server.request_count(), 2);
    assert_eq!(request_at(&server, 0).path, "/experimental/session");
    assert_eq!(request_at(&server, 1).path, "/session");
}

#[tokio::test]
async fn list_sessions_only_falls_back_on_404() {
    let server = TestHttpServer::start().await;
    server.route("GET", "/experimental/session", 500, r#"{"error":"boom"}"#);
    let client = v1_wire_client(&server, None);

    let err = client.list_sessions().await.unwrap_err();

    assert!(matches!(err, BridgeError::Http(_)), "unexpected: {err:?}");
    assert_eq!(server.request_count(), 1, "a 500 must not fall back to /session");
    assert_eq!(request_at(&server, 0).path, "/experimental/session");
}

#[tokio::test]
async fn update_session_title_patches_the_canonical_session_route() {
    let server = TestHttpServer::start().await;
    server.route("PATCH", "/session/ses_1", 200, r#"{"id":"ses_1"}"#);
    let client = v1_wire_client(&server, None);

    client.update_session_title("ses_1", "新标题").await.unwrap();

    let request = last_request(&server);
    assert_eq!(request.method, "PATCH");
    assert_eq!(request.path, "/session/ses_1");
    assert_eq!(request.query, "");
    assert_eq!(body_json(&request), serde_json::json!({"title": "新标题"}));
}

#[tokio::test]
async fn session_info_sends_the_directory_scope_and_parses_the_parent_chain() {
    let server = TestHttpServer::start().await;
    server.route(
        "GET",
        "/session/ses_child",
        200,
        serde_json::json!({
            "id": "ses_child",
            "parentID": "ses_parent",
            "title": "子会话",
            "model": {"providerID": "opencode-go", "id": "deepseek-v4-flash"},
        })
        .to_string(),
    );
    let client = v1_wire_client(&server, None);

    let info = client
        .session_info("ses_child", Some("/work/cola"))
        .await
        .unwrap();

    assert_eq!(info.id, "ses_child");
    assert_eq!(info.parent_id.as_deref(), Some("ses_parent"));
    assert_eq!(info.title.as_deref(), Some("子会话"));
    let model = info.model.as_ref().expect("model should parse");
    assert_eq!(model.provider_id, "opencode-go");
    assert_eq!(model.id, "deepseek-v4-flash");

    let request = last_request(&server);
    assert_eq!(request.method, "GET");
    assert_eq!(request.path, "/session/ses_child");
    assert_eq!(request.query_param("directory").as_deref(), Some("/work/cola"));

    client.session_info("ses_child", None).await.unwrap();
    assert_eq!(request_at(&server, 1).query, "", "no directory means no query");
}

#[tokio::test]
async fn session_status_parses_each_status_and_treats_an_absent_session_as_idle() {
    let server = TestHttpServer::start().await;
    server.route(
        "GET",
        "/session/status",
        200,
        serde_json::json!({
            "ses_idle": {"type": "idle"},
            "ses_busy": {"type": "busy"},
            "ses_retry": {"type": "retry", "attempt": 1, "message": "boom", "next": 5000},
            "ses_weird": {"type": "zombie"},
        })
        .to_string(),
    );
    let client = v1_wire_client(&server, None);

    assert_eq!(
        client
            .session_status("ses_idle", Some("/work/cola"))
            .await
            .unwrap(),
        Some(SessionStatus::Idle)
    );
    assert_eq!(
        client.session_status("ses_busy", None).await.unwrap(),
        Some(SessionStatus::Busy)
    );
    assert_eq!(
        client.session_status("ses_retry", None).await.unwrap(),
        Some(SessionStatus::Retry)
    );
    assert_eq!(client.session_status("ses_weird", None).await.unwrap(), None);
    assert_eq!(
        client.session_status("ses_absent", None).await.unwrap(),
        Some(SessionStatus::Idle)
    );

    assert_eq!(request_at(&server, 0).path, "/session/status");
    assert_eq!(
        request_at(&server, 0).query_param("directory").as_deref(),
        Some("/work/cola")
    );
    assert_eq!(request_at(&server, 1).query, "", "no directory means no query");
}

#[tokio::test]
async fn session_status_surfaces_http_and_decode_failures() {
    let down = TestHttpServer::start().await;
    down.route("GET", "/session/status", 500, r#"{"error":"boom"}"#);
    let client = v1_wire_client(&down, None);
    let message = opencode_error(client.session_status("ses_1", None).await.unwrap_err());
    assert!(message.contains("session status failed"), "unexpected: {message}");
    assert!(message.contains("500"), "unexpected: {message}");

    let garbled = TestHttpServer::start().await;
    garbled.route_raw("GET", "/session/status", 200, "text/html", "<html>nope</html>");
    let client = v1_wire_client(&garbled, None);
    let message = opencode_error(client.session_status("ses_1", None).await.unwrap_err());
    assert!(message.contains("session status parse"), "unexpected: {message}");
    assert!(message.contains("nope"), "unexpected: {message}");
}

#[tokio::test]
async fn list_permissions_sends_the_directory_scope_and_parses_requests() {
    let server = TestHttpServer::start().await;
    server.route(
        "GET",
        "/permission",
        200,
        serde_json::json!([{
            "id": "per_1",
            "sessionID": "ses_1",
            "permission": "bash",
            "patterns": ["rm -rf *"],
            "always": [],
            "metadata": {"command": "rm"},
        }])
        .to_string(),
    );
    let client = v1_wire_client(&server, None);

    let permissions = client.list_permissions(Some("/work/cola")).await.unwrap();

    assert_eq!(permissions.len(), 1);
    assert_eq!(permissions[0].request_id, "per_1");
    assert_eq!(permissions[0].session_id.as_deref(), Some("ses_1"));
    assert_eq!(permissions[0].permission.as_deref(), Some("bash"));
    assert_eq!(permissions[0].patterns, vec!["rm -rf *"]);

    let request = last_request(&server);
    assert_eq!(request.method, "GET");
    assert_eq!(request.path, "/permission");
    assert_eq!(request.query_param("directory").as_deref(), Some("/work/cola"));
}

#[tokio::test]
async fn list_permissions_maps_a_failed_status_to_a_diagnostic_opencode_error() {
    let server = TestHttpServer::start().await;
    server.route("GET", "/permission", 502, r#"{"error":"bad gateway"}"#);
    let client = v1_wire_client(&server, None);

    let message = opencode_error(client.list_permissions(None).await.unwrap_err());

    assert!(
        message.contains("permission list failed"),
        "unexpected: {message}"
    );
    assert!(message.contains("502"), "unexpected: {message}");
}

#[tokio::test]
async fn reply_permission_posts_the_reply_with_the_directory_scope() {
    let server = TestHttpServer::start().await;
    server.route("POST", "/permission/per_1/reply", 200, r#"{"code":0}"#);
    server.route(
        "POST",
        "/permission/per_gone/reply",
        404,
        r#"{"error":"not found"}"#,
    );
    let client = v1_wire_client(&server, None);

    client
        .reply_permission("per_1", "always", Some("/work/cola"))
        .await
        .unwrap();

    let request = last_request(&server);
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, "/permission/per_1/reply");
    assert_eq!(request.query_param("directory").as_deref(), Some("/work/cola"));
    assert_eq!(body_json(&request), serde_json::json!({"reply": "always"}));

    // A 404 means the request was already resolved elsewhere — the benign
    // NotFound variant, not a transport failure.
    let message = not_found_error(
        client
            .reply_permission("per_gone", "once", None)
            .await
            .unwrap_err(),
    );
    assert!(message.contains("permission per_gone"), "unexpected: {message}");
}

#[tokio::test]
async fn list_questions_sends_the_directory_scope_and_parses_questions() {
    let server = TestHttpServer::start().await;
    server.route(
        "GET",
        "/question",
        200,
        serde_json::json!([{
            "id": "q_1",
            "sessionID": "ses_1",
            "questions": [{
                "question": "选哪个？",
                "header": "选择",
                "options": [
                    {"label": "A", "description": "first"},
                    {"label": "B", "description": "second"},
                ],
                "multiple": true,
                "custom": false,
            }],
        }])
        .to_string(),
    );
    let client = v1_wire_client(&server, None);

    let questions = client.list_questions(Some("/work/cola")).await.unwrap();

    assert_eq!(questions.len(), 1);
    assert_eq!(questions[0].id, "q_1");
    assert_eq!(questions[0].session_id, "ses_1");
    assert_eq!(questions[0].questions[0].question, "选哪个？");
    assert_eq!(questions[0].questions[0].options[0].label, "A");
    assert_eq!(questions[0].questions[0].multiple, Some(true));

    let request = last_request(&server);
    assert_eq!(request.method, "GET");
    assert_eq!(request.path, "/question");
    assert_eq!(request.query_param("directory").as_deref(), Some("/work/cola"));
}

#[tokio::test]
async fn reply_question_posts_answers_with_the_directory_scope() {
    let server = TestHttpServer::start().await;
    server.route("POST", "/question/q_1/reply", 200, r#"{"code":0}"#);
    server.route("POST", "/question/q_gone/reply", 404, r#"{"error":"not found"}"#);
    let client = v1_wire_client(&server, None);

    client
        .reply_question(
            "q_1",
            &[vec!["A".to_string()], vec!["B".to_string(), "C".to_string()]],
            Some("/work/cola"),
        )
        .await
        .unwrap();

    let request = last_request(&server);
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, "/question/q_1/reply");
    assert_eq!(request.query_param("directory").as_deref(), Some("/work/cola"));
    assert_eq!(
        body_json(&request),
        serde_json::json!({"answers": [["A"], ["B", "C"]]})
    );

    let message = not_found_error(client.reply_question("q_gone", &[], None).await.unwrap_err());
    assert!(message.contains("question q_gone"), "unexpected: {message}");
}

/// A hung request-reply endpoint must not hold its caller (a card click's
/// handler) forever: the call gives up on its own and the bridge renders a
/// retryable failure. Prompt POSTs deliberately keep running for minutes —
/// only these reply endpoints are bounded.
#[tokio::test(start_paused = true)]
async fn reply_endpoints_give_up_on_a_hung_server() {
    let server = TestHttpServer::start().await;
    let hang = std::time::Duration::from_secs(60);
    server.route_delayed("POST", "/question/q_1/reply", 200, r#"{"code":0}"#, hang);
    server.route_delayed("POST", "/question/q_1/reject", 200, r#"{"code":0}"#, hang);
    server.route_delayed("POST", "/permission/p_1/reply", 200, r#"{"code":0}"#, hang);
    let client = v1_wire_client(&server, None);

    for (label, err) in [
        (
            "reply",
            client.reply_question("q_1", &[], None).await.unwrap_err(),
        ),
        ("reject", client.reject_question("q_1", None).await.unwrap_err()),
        (
            "permission",
            client.reply_permission("p_1", "once", None).await.unwrap_err(),
        ),
    ] {
        assert!(
            matches!(err, BridgeError::Http(ref e) if e.is_timeout()),
            "{label} did not time out: {err:?}"
        );
    }
}

#[tokio::test]
async fn reject_question_posts_with_the_directory_scope() {
    let server = TestHttpServer::start().await;
    server.route("POST", "/question/q_1/reject", 200, r#"{"code":0}"#);
    server.route("POST", "/question/q_gone/reject", 404, r#"{"error":"not found"}"#);
    let client = v1_wire_client(&server, None);

    client.reject_question("q_1", Some("/work/cola")).await.unwrap();

    let request = last_request(&server);
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, "/question/q_1/reject");
    assert_eq!(request.query_param("directory").as_deref(), Some("/work/cola"));
    assert_eq!(request.body, "", "reject carries no body");

    let message = not_found_error(client.reject_question("q_gone", None).await.unwrap_err());
    assert!(message.contains("question q_gone"), "unexpected: {message}");
}

/// The legacy payload shapes a session read serves: a user message, an
/// assistant turn with reasoning/text/tool(/unknown)/patch/step-finish
/// parts, and an in-flight assistant message with no completion stamp.
fn transcript_fixture() -> serde_json::Value {
    serde_json::json!([
        {
            "info": {"id": "msg_u1", "role": "user", "time": {"created": 1000}},
            "parts": [{"type": "text", "text": "第一个问题"}]
        },
        {
            "info": {
                "id": "msg_a1",
                "role": "assistant",
                "time": {"created": 1100, "completed": 1400},
                "modelID": "deepseek-v4-flash",
                "providerID": "opencode-go",
                "tokens": {"total": 162034, "input": 220, "output": 129,
                           "cache": {"write": 0, "read": 161664}}
            },
            "parts": [
                {"type": "step-start", "snapshot": "abc"},
                {"type": "reasoning", "text": "想一下", "time": {"start": 1110, "end": 1120}},
                {"type": "text", "text": "答案一", "time": {"start": 1120, "end": 1130}},
                {"type": "tool", "tool": "bash", "callID": "call_1",
                 "state": {"status": "completed", "input": {"command": "ls"},
                           "output": "src", "metadata": {"output": ""},
                           "time": {"start": 1130, "end": 1140}}},
                {"type": "tool", "tool": "mystery", "callID": "call_2",
                 "state": {"status": "weird", "input": {"x": 1}}},
                // A null `output` must not shadow the content/result this
                // state actually carries (the pre-fix regression).
                {"type": "tool", "tool": "read", "callID": "call_3",
                 "state": {"status": "completed", "input": {"filePath": "a.rs"},
                           "output": null,
                           "content": [{"type": "text", "text": "line1"},
                                       {"type": "text", "text": "line2"}],
                           "result": "2 lines"}},
                {"type": "mystery-part", "payload": 42},
                {"type": "patch", "hash": "abc", "files": ["src/a.rs"]},
                {"type": "step-finish", "reason": "tool-calls"}
            ]
        },
        {
            "info": {"id": "msg_a2", "role": "assistant", "time": {"created": 2000}},
            "parts": [{"type": "text", "text": "答案二", "time": {"start": 2010}}]
        }
    ])
}

/// The adapter produces the neutral Session Transcript from the legacy
/// wire payloads: the request goes to the canonical message route and
/// every typed view (message envelope, part kinds, tool payloads, tolerant
/// arms) decodes, with the projections agreeing with the raw read.
#[tokio::test]
async fn transcript_decodes_legacy_payloads_through_the_adapter() {
    use crate::backend::Backend;
    use crate::backend::{ContentBlock, FinishReason, MessageRole, MessageTime, Part, ToolStatus};

    let server = TestHttpServer::start().await;
    server.route(
        "GET",
        "/session/ses_1/message",
        200,
        transcript_fixture().to_string(),
    );
    let backend: std::sync::Arc<dyn Backend> = std::sync::Arc::new(v1_wire_client(&server, None));

    let transcript = backend.transcript("ses_1").await.unwrap();

    let request = last_request(&server);
    assert_eq!(request.method, "GET");
    assert_eq!(request.path, "/session/ses_1/message");
    assert_eq!(transcript.messages.len(), 3);

    let user = &transcript.messages[0];
    assert_eq!(user.id.as_str(), "msg_u1");
    assert_eq!(user.role, MessageRole::User);
    assert_eq!(
        user.time,
        Some(MessageTime {
            created: 1000,
            completed: None
        })
    );
    assert_eq!(user.text(), "第一个问题");

    let assistant = &transcript.messages[1];
    assert_eq!(assistant.role, MessageRole::Assistant);
    let model = assistant.model.as_ref().expect("the message names its model");
    assert_eq!(model.provider_id, "opencode-go");
    assert_eq!(model.model_id, "deepseek-v4-flash");
    assert!(model.variant.is_none());
    assert_eq!(assistant.tokens.as_ref().unwrap().context_used(), 162034);

    let Part::Reasoning(reasoning) = &assistant.parts[1] else {
        panic!("expected a reasoning part: {:?}", assistant.parts[1]);
    };
    assert_eq!(reasoning.text, "想一下");
    assert_eq!(reasoning.started_at, Some(1110));
    let Part::Text(text) = &assistant.parts[2] else {
        panic!("expected a text part: {:?}", assistant.parts[2]);
    };
    assert_eq!(text.text, "答案一");
    assert_eq!(text.started_at, Some(1120));

    let Part::Tool(call) = &assistant.parts[3] else {
        panic!("expected a tool part: {:?}", assistant.parts[3]);
    };
    assert_eq!(call.identity.name, "bash");
    assert_eq!(call.identity.call_id, "call_1");
    assert_eq!(call.status, ToolStatus::Completed);
    assert_eq!(call.started_at, Some(1130));
    assert_eq!(call.input.as_ref().unwrap()["command"].as_str(), Some("ls"));
    assert_eq!(call.metadata.as_ref().unwrap()["output"].as_str(), Some(""));
    assert_eq!(call.output.raw.as_ref().unwrap().as_str(), Some("src"));
    assert_eq!(call.output.blocks, vec![ContentBlock::Text("src".into())]);
    assert!(call.output.error.is_none());

    // An unrecognized tool status decodes tolerantly, payloads intact.
    let Part::Tool(unknown) = &assistant.parts[4] else {
        panic!("expected a tool part: {:?}", assistant.parts[4]);
    };
    assert_eq!(unknown.status, ToolStatus::Other("weird".into()));
    assert!(unknown.started_at.is_none());
    assert!(unknown.output.raw.is_none());
    assert_eq!(unknown.input.as_ref().unwrap()["x"], 1);

    // A null output does not shadow the content/result sources: the text
    // runs and the result join into one block and the raw payload is the
    // content the state carried.
    let Part::Tool(null_output) = &assistant.parts[5] else {
        panic!("expected a tool part: {:?}", assistant.parts[5]);
    };
    assert_eq!(
        null_output.output.blocks,
        vec![ContentBlock::Text("line1line2\n2 lines".into())]
    );
    assert_eq!(
        null_output.output.raw.as_ref().unwrap(),
        &serde_json::json!([
            {"type": "text", "text": "line1"},
            {"type": "text", "text": "line2"}
        ])
    );

    // An unrecognized part kind keeps its raw payload.
    let Part::Other(other) = &assistant.parts[6] else {
        panic!("expected an Other part: {:?}", assistant.parts[6]);
    };
    assert_eq!(other.kind, "mystery-part");
    assert_eq!(other.raw["payload"], 42);

    let Part::Patch(patch) = &assistant.parts[7] else {
        panic!("expected a patch part: {:?}", assistant.parts[7]);
    };
    assert_eq!(patch.hash.as_deref(), Some("abc"));
    assert_eq!(patch.files, vec!["src/a.rs"]);

    let Part::StepFinish(finish) = &assistant.parts[8] else {
        panic!("expected a step-finish part: {:?}", assistant.parts[8]);
    };
    assert_eq!(finish.reason, FinishReason::ToolCalls);

    // Projections read the decoded transcript: the anchor is the user
    // message's identity + server time, and the in-flight assistant
    // message belongs to the turn, which is not complete on `tool-calls`.
    let newest = transcript.newest_user().expect("a user message exists");
    let anchor = newest.anchor().expect("the user message has a server time");
    assert_eq!(anchor.message_id.as_str(), "msg_u1");
    assert_eq!(anchor.created_ms, 1000);
    let turn = transcript.turn_for_user(&anchor);
    let ids: Vec<_> = turn.messages.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, vec!["msg_a1", "msg_a2"], "the in-flight message must belong");
    assert!(!turn.complete, "tool-calls is not a terminal finish");
}

/// A terminal `step-finish` in the decoded turn reports completion, and a
/// pre-anchor message stays the previous turn's.
#[tokio::test]
async fn transcript_turn_completes_on_a_terminal_finish() {
    let server = TestHttpServer::start().await;
    server.route(
        "GET",
        "/session/ses_done/message",
        200,
        serde_json::json!([
            {"info": {"id": "msg_u1", "role": "user", "time": {"created": 1000}},
             "parts": [{"type": "text", "text": "hi"}]},
            // The previous turn: finished before the anchor.
            {"info": {"id": "msg_old", "role": "assistant", "time": {"created": 500, "completed": 600}},
             "parts": [{"type": "step-finish", "reason": "stop"}]},
            {"info": {"id": "msg_a1", "role": "assistant", "time": {"created": 1100, "completed": 1200}},
             "parts": [{"type": "step-finish", "reason": "stop"}]}
        ])
        .to_string(),
    );
    let client = v1_wire_client(&server, None);

    let transcript = client.transcript("ses_done").await.unwrap();
    let anchor = transcript.newest_user().unwrap().anchor().unwrap();
    let turn = transcript.turn_for_user(&anchor);

    let ids: Vec<_> = turn.messages.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, vec!["msg_a1"], "a pre-anchor message is the previous turn's");
    assert!(turn.complete);
}

/// A failed message read surfaces as an error; the transcript read adds no
/// silent fallback.
#[tokio::test]
async fn transcript_surfaces_a_failed_message_read() {
    let server = TestHttpServer::start().await;
    server.route("GET", "/session/ses_gone/message", 500, r#"{"error":"boom"}"#);
    let client = v1_wire_client(&server, None);

    assert!(client.transcript("ses_gone").await.is_err());
}
