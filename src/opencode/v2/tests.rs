//! The V2 suite: wire tests driving the real adapter (with the V2 strategy
//! selected) against a local fake server, asserting both sides of every
//! exchange — the recorded request (method / path / query / body) and what the
//! client parsed back (ADR-0031). These pin the V2 read contract: the `{data}`
//! envelopes, the body cursor, the 204 mutations, and the run-state derivation.

use super::V2Strategy;
use crate::error::BridgeError;
use crate::opencode::client::OpenCodeBackend;
use crate::opencode::strategy::{Generation, GenerationStrategy};
use crate::opencode::transport::Transport;
use crate::opencode::types::SessionStatus;
use crate::test_http::{MockResponse, RecordedRequest, TestHttpServer};

/// A client pointed at the fake server with both Basic-auth parts set, speaking
/// V2 — what attach detection hands production for a `/api` server. The
/// transport is swapped for a no-proxy one so a developer shell's `http_proxy`
/// cannot intercept the loopback fake server (ticket 09); construction itself
/// stays production's.
fn v2_wire_client(server: &TestHttpServer) -> OpenCodeBackend {
    let client = OpenCodeBackend::with_generation(
        None,
        server.base_url(),
        Some("opencode"),
        Some("secret"),
        Generation::V2,
        None,
    );
    client.disable_env_proxy(Some("opencode"), Some("secret"));
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

fn opencode_error(err: BridgeError) -> String {
    match err {
        BridgeError::OpenCode(message) => message,
        other => panic!("expected BridgeError::OpenCode, got: {other:?}"),
    }
}

/// The V2 list envelope: entries expose the directory in `location.directory`
/// (not V1's top-level field) and the cursor in the body. The follow-up loop
/// walks `cursor.next` to the first empty page, because the server emits `next`
/// for every non-empty page.
#[tokio::test]
async fn list_sessions_unwraps_the_data_envelope_and_follows_the_body_cursor() {
    let server = TestHttpServer::start().await;
    server.route_sequence(
        "GET",
        "/api/session",
        vec![
            MockResponse::json(
                serde_json::json!({
                    "data": [
                        {
                            "id": "ses_new",
                            "title": "新",
                            "parentID": "ses_parent",
                            "agent": "build",
                            "model": {"id": "deepseek-v4-flash", "providerID": "opencode-go"},
                            "location": {"directory": "/work/cola"},
                            "time": {"created": 1700000000000i64, "updated": 1700000200000i64},
                        },
                        {
                            "id": "ses_child",
                            "title": "子会话",
                            "location": {"directory": "/work/cola"},
                            "time": {"created": 1700000000000i64, "updated": 1700000150000i64},
                        }
                    ],
                    "cursor": {"previous": "c_prev", "next": "c1"},
                })
                .to_string(),
            ),
            MockResponse::json(
                serde_json::json!({
                    "data": [{
                        "id": "ses_old",
                        "title": "旧",
                        "location": {"directory": "/work/other"},
                        "time": {"created": 1700000000000i64, "updated": 1700000100000i64},
                    }],
                    "cursor": {"previous": "c1", "next": "c2"},
                })
                .to_string(),
            ),
            MockResponse::json(serde_json::json!({"data": [], "cursor": {"previous": "c2"}}).to_string()),
        ],
    );
    let client = v2_wire_client(&server);

    let sessions = client.list_sessions().await.unwrap();

    let ids: Vec<&str> = sessions.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(
        ids,
        ["ses_new", "ses_child", "ses_old"],
        "every page merged in order"
    );
    assert_eq!(
        sessions[0].directory, "/work/cola",
        "location.directory maps to directory"
    );
    assert_eq!(sessions[2].directory, "/work/other");
    assert_eq!(sessions[0].title, "新");
    assert_eq!(sessions[0].parent_id.as_deref(), Some("ses_parent"));
    assert!(sessions[0].is_child());
    assert_eq!(sessions[0].model.as_ref().unwrap()["providerID"], "opencode-go");
    assert_eq!(
        sessions[0].time.as_ref().unwrap().updated,
        1700000200000,
        "V2's epoch-millis time decodes into the neutral row"
    );
    assert_eq!(
        server.request_count(),
        3,
        "two pages plus the terminating empty page"
    );
    assert_eq!(request_at(&server, 0).path, "/api/session");
    assert_eq!(
        request_at(&server, 0).query,
        "",
        "the first page carries no cursor"
    );
    assert_eq!(
        request_at(&server, 1).query_param("cursor").as_deref(),
        Some("c1"),
        "the follow-up sends the body cursor"
    );
    assert_eq!(
        request_at(&server, 2).query_param("cursor").as_deref(),
        Some("c2")
    );
}

/// The V2 list read has no fallback: a missing route is a loud error, and a
/// bare array body (the V1 shape) is a decode error, not a silently empty
/// list.
#[tokio::test]
async fn list_sessions_does_not_fall_back_on_404_or_accept_a_bare_array() {
    let server = TestHttpServer::start().await;
    server.route(
        "GET",
        "/api/session",
        404,
        r#"{"_tag":"NotFoundError","message":"no"}"#,
    );
    let client = v2_wire_client(&server);

    assert!(client.list_sessions().await.is_err());
    assert_eq!(
        server.request_count(),
        1,
        "a 404 must not fall back to another route"
    );

    let bare = TestHttpServer::start().await;
    bare.route("GET", "/api/session", 200, "[]");
    let client = v2_wire_client(&bare);
    assert!(
        client.list_sessions().await.is_err(),
        "V2 wraps the list in {{data}}; a bare array is a decode error"
    );
}

/// `GET /api/session/{id}` unwraps `{data}` and carries the parent chain and
/// the server-recorded model the effective-model ladder reads. The session id
/// resolves globally on V2, so the directory handle is not sent on the wire.
#[tokio::test]
async fn session_info_unwraps_the_data_envelope_and_sends_no_directory_scope() {
    let server = TestHttpServer::start().await;
    server.route(
        "GET",
        "/api/session/ses_child",
        200,
        serde_json::json!({
            "data": {
                "id": "ses_child",
                "parentID": "ses_parent",
                "title": "子会话",
                "model": {"providerID": "opencode-go", "id": "deepseek-v4-flash"},
                "location": {"directory": "/work/cola"},
                "time": {"created": 1, "updated": 2},
            },
        })
        .to_string(),
    );
    let client = v2_wire_client(&server);

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
    assert_eq!(request.path, "/api/session/ses_child");
    assert_eq!(
        request.query, "",
        "V2 resolves the session id globally; no directory scope is sent"
    );
}

/// The 204 mutations: V2's PATCH and DELETE answer no content, so the client
/// must not expect (or attempt to parse) a success body.
#[tokio::test]
async fn update_title_and_delete_accept_204_without_a_success_body() {
    let server = TestHttpServer::start().await;
    server.route("PATCH", "/api/session/ses_1", 204, "");
    server.route("DELETE", "/api/session/ses_1", 204, "");
    let client = v2_wire_client(&server);

    client.update_session_title("ses_1", "新标题").await.unwrap();
    let patch = last_request(&server);
    assert_eq!(patch.method, "PATCH");
    assert_eq!(patch.path, "/api/session/ses_1");
    assert_eq!(body_json(&patch), serde_json::json!({"title": "新标题"}));

    client.delete_session("ses_1").await.unwrap();
    let delete = last_request(&server);
    assert_eq!(delete.method, "DELETE");
    assert_eq!(delete.path, "/api/session/ses_1");
    assert_eq!(delete.query, "", "delete carries no scope");
    assert_eq!(delete.body, "", "delete carries no body");
}

/// The run-state read: `session.active` says only `{type:"running"}`; absence
/// is idle; an active session's newest assistant message's `retry` field maps
/// to Retry ahead of Running. The retry read is paid only for active sessions.
#[tokio::test]
async fn session_status_derives_retry_from_the_newest_assistant_message() {
    let server = TestHttpServer::start().await;
    server.route(
        "GET",
        "/api/session/active",
        200,
        serde_json::json!({
            "data": {
                "ses_busy": {"type": "running"},
                "ses_retry": {"type": "running"},
                "ses_weird": {"type": "zombie"},
            },
        })
        .to_string(),
    );
    server.route(
        "GET",
        "/api/session/ses_retry/message",
        200,
        serde_json::json!({
            "data": [{"type": "assistant", "retry": {"attempt": 2, "at": 5000, "error": {"message": "boom"}}}],
            "cursor": {},
        })
        .to_string(),
    );
    // Every other session's message read: an assistant with the retry field
    // cleared (V2 serializes it as null).
    server.route(
        "GET",
        "/api/session/",
        200,
        serde_json::json!({"data": [{"type": "assistant", "retry": null}], "cursor": {}}).to_string(),
    );
    let client = v2_wire_client(&server);

    assert_eq!(
        client
            .session_status("ses_retry", Some("/work/cola"))
            .await
            .unwrap(),
        Some(SessionStatus::Retry),
        "a scheduled retry is Retry while the session is active"
    );
    assert_eq!(
        client.session_status("ses_busy", None).await.unwrap(),
        Some(SessionStatus::Busy)
    );
    assert_eq!(
        client.session_status("ses_idle", None).await.unwrap(),
        Some(SessionStatus::Idle),
        "absence from the active map is idle"
    );
    assert_eq!(
        client.session_status("ses_weird", None).await.unwrap(),
        None,
        "an unrecognised active type is never guessed"
    );

    // The active map is the first and only read for idle/unknown sessions; an
    // active one pays one more request for the newest assistant message.
    let active = request_at(&server, 0);
    assert_eq!(active.path, "/api/session/active");
    assert_eq!(active.query, "", "the active map is global; no directory scope");
    let retry_read = request_at(&server, 1);
    assert_eq!(retry_read.path, "/api/session/ses_retry/message");
    assert_eq!(retry_read.query_param("type").as_deref(), Some("assistant"));
    assert_eq!(retry_read.query_param("order").as_deref(), Some("desc"));
    assert_eq!(retry_read.query_param("limit").as_deref(), Some("1"));
    assert_eq!(request_at(&server, 2).path, "/api/session/active");
    assert_eq!(request_at(&server, 3).path, "/api/session/ses_busy/message");
    assert_eq!(request_at(&server, 4).path, "/api/session/active");
    assert_eq!(request_at(&server, 5).path, "/api/session/active");
    assert_eq!(
        server.request_count(),
        6,
        "retry=2, busy=2, idle=1 (no retry read), weird=1 (no retry read)"
    );
}

/// Both status reads surface their failures: the active map's non-success and
/// garbled body hard-fail, while a failed retry read degrades to Running with
/// a warning (the active map already answered the primary question).
#[tokio::test]
async fn session_status_surfaces_failures() {
    let failed = TestHttpServer::start().await;
    failed.route("GET", "/api/session/active", 500, r#"{"message":"boom"}"#);
    let client = v2_wire_client(&failed);
    let message = opencode_error(client.session_status("ses_1", None).await.unwrap_err());
    assert!(message.contains("session status failed"), "unexpected: {message}");
    assert!(message.contains("500"), "unexpected: {message}");

    let garbled = TestHttpServer::start().await;
    garbled.route_raw(
        "GET",
        "/api/session/active",
        200,
        "text/html",
        "<html>nope</html>",
    );
    let client = v2_wire_client(&garbled);
    let message = opencode_error(client.session_status("ses_1", None).await.unwrap_err());
    assert!(message.contains("session status parse"), "unexpected: {message}");
    assert!(message.contains("nope"), "unexpected: {message}");

    // A failed retry read never fails the status: the active map said running.
    let retry_down = TestHttpServer::start().await;
    retry_down.route(
        "GET",
        "/api/session/active",
        200,
        serde_json::json!({"data": {"ses_1": {"type": "running"}}}).to_string(),
    );
    retry_down.route("GET", "/api/session/ses_1/message", 500, r#"{"message":"boom"}"#);
    let client = v2_wire_client(&retry_down);
    let (status, logs) =
        crate::bridge::test_support::capture_logs(async { client.session_status("ses_1", None).await }).await;
    assert_eq!(
        status.unwrap(),
        Some(SessionStatus::Busy),
        "the failed retry read degrades to Running"
    );
    let warning = crate::bridge::test_support::assert_line_level(&logs, "session retry read failed", "WARN");
    assert!(
        warning.contains("session ses_1") && warning.contains("500"),
        "the warning names the session and the status: {warning}"
    );

    // The same applies to a garbled retry read.
    let retry_garbled = TestHttpServer::start().await;
    retry_garbled.route(
        "GET",
        "/api/session/active",
        200,
        serde_json::json!({"data": {"ses_1": {"type": "running"}}}).to_string(),
    );
    retry_garbled.route_raw(
        "GET",
        "/api/session/ses_1/message",
        200,
        "text/html",
        "<html>nope</html>",
    );
    let client = v2_wire_client(&retry_garbled);
    let (status, logs) =
        crate::bridge::test_support::capture_logs(async { client.session_status("ses_1", None).await }).await;
    assert_eq!(status.unwrap(), Some(SessionStatus::Busy));
    let warning = crate::bridge::test_support::assert_line_level(&logs, "session retry read parse", "WARN");
    assert!(
        warning.contains("nope"),
        "the warning carries the body: {warning}"
    );
}

/// Attaching to V2 must never pretend a missing capability worked: a caller
/// gets an error naming the method and the generation (spec #364, S3). S4a
/// landed the session reads, so this pins a capability still waiting for its
/// slice (transcript is S4b).
#[tokio::test]
async fn remaining_capabilities_fail_loudly_until_their_slice_lands() {
    let strategy = V2Strategy;
    let http = Transport::new(Some("opencode"), Some("pw"), "http://127.0.0.1:1");
    let error = strategy.transcript(&http, "ses_1").await.unwrap_err().to_string();
    assert!(error.contains("V2 strategy"), "unexpected: {error}");
    assert!(error.contains("transcript"), "unexpected: {error}");
    assert!(error.contains("not implemented"), "unexpected: {error}");
    // The two catalog reads degrade to empty with a warning instead of an
    // error (their card surfaces tolerate emptiness).
    assert!(strategy.list_agents(&http).await.is_empty());
    assert!(strategy.list_models(&http).await.is_empty());
}
