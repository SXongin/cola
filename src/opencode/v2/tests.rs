//! The V2 suite: wire tests driving the real adapter (with the V2 strategy
//! selected) against a local fake server, asserting both sides of every
//! exchange — the recorded request (method / path / query / body) and what the
//! client parsed back (ADR-0031). These pin the V2 read contract: the `{data}`
//! envelopes, the body cursor, the transcript decode and its pagination, the
//! 204 mutations, and the run-state derivation.

use crate::error::BridgeError;
use crate::opencode::client::OpenCodeBackend;
use crate::opencode::strategy::Generation;
use crate::opencode::types::{ImageInput, SessionStatus};
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

/// The transcript read: `order=asc` on the first page, then `cursor.next`
/// follow-ups that MUST NOT repeat `order` (V2 answers `InvalidCursorError`
/// otherwise), decoded through the strategy into the neutral transcript. The
/// server mints `next` for every non-empty page, so the follow ends on the
/// first empty one.
#[tokio::test]
async fn transcript_follows_the_body_cursor_in_ascending_order() {
    let server = TestHttpServer::start().await;
    server.route_sequence(
        "GET",
        "/api/session/ses_1/message",
        vec![
            MockResponse::json(
                serde_json::json!({
                    "data": [
                        {
                            "id": "msg_u1",
                            "type": "user",
                            "time": {"created": 1700000000000i64},
                            "text": "问题",
                        },
                        {
                            "id": "msg_a1",
                            "type": "assistant",
                            "time": {"created": 1700000000100i64, "completed": 1700000000200i64},
                            "model": {"id": "deepseek-v4-flash", "providerID": "opencode-go"},
                            "tokens": {"input": 11, "output": 7, "reasoning": 0, "cache": {"read": 0, "write": 0}},
                            "content": [
                                {"type": "reasoning", "text": "想想", "time": {"created": 1700000000120i64}},
                                {"type": "tool", "id": "call_1", "name": "shell",
                                 "state": {"status": "completed", "input": {"command": "echo hi"},
                                           "content": [{"type": "text", "text": "hi\n"}]},
                                 "time": {"created": 1700000000130i64, "completed": 1700000000190i64}},
                                {"type": "text", "text": "答案"},
                            ],
                            "finish": "stop",
                        },
                    ],
                    "cursor": {"next": "c1"},
                })
                .to_string(),
            ),
            MockResponse::json(
                serde_json::json!({
                    "data": [{"id": "msg_i1", "type": "idle", "time": {"created": 1700000000300i64}, "outcome": "succeeded"}],
                    "cursor": {"next": "c2"},
                })
                .to_string(),
            ),
            MockResponse::json(serde_json::json!({"data": [], "cursor": {"previous": "c2"}}).to_string()),
        ],
    );
    let client = v2_wire_client(&server);

    let transcript = client.transcript("ses_1").await.unwrap();

    let ids: Vec<&str> = transcript.messages.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(
        ids,
        ["msg_u1", "msg_a1", "msg_i1"],
        "every page merged in server order"
    );
    let user = transcript
        .newest_user()
        .expect("the user message anchors the turn");
    assert_eq!(user.text(), "问题");
    let anchor = user.anchor().expect("the user message carries a server time");
    assert_eq!(anchor.created_ms, 1700000000000);
    let turn = transcript.turn_for_user(&anchor);
    assert!(turn.complete, "the terminal finish ends the turn");
    assert_eq!(turn.messages.len(), 1, "only the assistant message belongs");
    let tool_part = turn.messages[0]
        .parts
        .iter()
        .find_map(|part| match part {
            crate::backend::Part::Tool(tool) => Some(tool),
            _ => None,
        })
        .expect("the tool content decodes into a typed call");
    assert_eq!(tool_part.identity.name, "shell");
    assert_eq!(tool_part.identity.call_id, "call_1");
    assert_eq!(tool_part.status, crate::backend::ToolStatus::Completed);
    assert_eq!(tool_part.input.as_ref().unwrap()["command"], "echo hi");
    assert_eq!(
        tool_part.output.blocks,
        vec![crate::backend::ContentBlock::Text("hi\n".into())]
    );

    assert_eq!(
        server.request_count(),
        3,
        "two pages plus the terminating empty page"
    );
    let first = request_at(&server, 0);
    assert_eq!(first.path, "/api/session/ses_1/message");
    assert_eq!(first.query_param("order").as_deref(), Some("asc"));
    assert_eq!(first.query_param("limit").as_deref(), Some("200"));
    assert_eq!(first.query_param("cursor"), None, "the first page has no cursor");
    for (index, cursor) in [(1, "c1"), (2, "c2")] {
        let request = request_at(&server, index);
        assert_eq!(request.query_param("cursor").as_deref(), Some(cursor));
        assert_eq!(request.query_param("limit").as_deref(), Some("200"));
        assert_eq!(
            request.query_param("order"),
            None,
            "a cursor must never be combined with order (V2 answers InvalidCursorError)"
        );
    }
}

/// A failed transcript read names itself and carries the body preview; a
/// garbled body is a parse error, never a silently empty transcript.
#[tokio::test]
async fn transcript_surfaces_read_failures() {
    let failed = TestHttpServer::start().await;
    failed.route("GET", "/api/session/ses_1/message", 500, r#"{"message":"boom"}"#);
    let client = v2_wire_client(&failed);
    let message = opencode_error(client.transcript("ses_1").await.unwrap_err());
    assert!(
        message.contains("transcript read failed"),
        "unexpected: {message}"
    );
    assert!(
        message.contains("500") && message.contains("boom"),
        "unexpected: {message}"
    );

    let garbled = TestHttpServer::start().await;
    garbled.route_raw(
        "GET",
        "/api/session/ses_1/message",
        200,
        "text/html",
        "<html>nope</html>",
    );
    let client = v2_wire_client(&garbled);
    let message = opencode_error(client.transcript("ses_1").await.unwrap_err());
    assert!(message.contains("transcript read parse"), "unexpected: {message}");
    assert!(message.contains("nope"), "unexpected: {message}");
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

/// The admitted-prompt response body (`{data: SessionInbox.User}`); the write
/// path reads only `data.id`.
fn admitted_body(id: &str) -> String {
    serde_json::json!({
        "data": {
            "id": id,
            "sessionID": "ses_1",
            "type": "user",
            "delivery": "steer",
            "time": {"created": 1},
            "payload": {"text": "hi"},
        },
    })
    .to_string()
}

/// The prompt submit is V2's native admit-then-return: one POST durably
/// admits the message (cola's id, text + files, explicit `steer` delivery) and
/// returns immediately — no wait endpoint, no transcript read, no polling. The
/// Turn observes the turn it scheduled from the transcript + run state
/// (ADR-0056).
#[tokio::test]
async fn prompt_admits_the_text_files_and_id_then_returns_without_waiting() {
    let server = TestHttpServer::start().await;
    server.route(
        "POST",
        "/api/session/ses_1/prompt",
        200,
        admitted_body("msg_cola_1"),
    );
    let client = v2_wire_client(&server);

    client
        .prompt(
            "ses_1",
            "hi",
            &[ImageInput {
                mime: "image/png".into(),
                data_base64: "AAAA".into(),
            }],
            None,
            None,
            None,
            Some("msg_cola_1"),
        )
        .await
        .unwrap();

    assert_eq!(
        server.request_count(),
        1,
        "admit only: the submit must not wait or read the transcript"
    );
    let prompt = request_at(&server, 0);
    assert_eq!(prompt.method, "POST");
    assert_eq!(prompt.path, "/api/session/ses_1/prompt");
    assert_eq!(
        body_json(&prompt),
        serde_json::json!({
            "id": "msg_cola_1",
            "text": "hi",
            "files": [{"uri": "data:image/png;base64,AAAA"}],
            "delivery": "steer",
        }),
        "V2's prompt payload: text + files + the durable id + explicit steer; no model/agent axes"
    );
}

/// A prompt without images carries no `files` key at all (never an empty
/// array), and a prompt without a cola id omits `id` so the server mints one.
#[tokio::test]
async fn prompt_omits_empty_files_and_an_absent_message_id() {
    let server = TestHttpServer::start().await;
    server.route("POST", "/api/session/ses_1/prompt", 200, admitted_body("msg_srv"));
    let client = v2_wire_client(&server);

    client
        .prompt("ses_1", "hi", &[], None, None, None, None)
        .await
        .unwrap();

    assert_eq!(
        body_json(&request_at(&server, 0)),
        serde_json::json!({"text": "hi", "delivery": "steer"}),
        "no files, no id"
    );
}

/// A 404 is the bridge's recreate-the-mapping signal only when its `_tag` says
/// the session is missing: an untagged proxy 404 stays a plain failure.
#[tokio::test]
async fn prompt_maps_only_a_tagged_session_not_found() {
    let tagged = TestHttpServer::start().await;
    tagged.route(
        "POST",
        "/api/session/ses_gone/prompt",
        404,
        r#"{"_tag":"SessionNotFoundError","message":"gone"}"#,
    );
    let client = v2_wire_client(&tagged);
    let error = client
        .prompt("ses_gone", "hi", &[], None, None, None, None)
        .await
        .unwrap_err();
    assert!(
        matches!(error, BridgeError::SessionNotFound(_)),
        "a tagged 404 is SessionNotFound: {error:?}"
    );

    let untagged = TestHttpServer::start().await;
    untagged.route(
        "POST",
        "/api/session/ses_1/prompt",
        404,
        r#"{"message":"no route"}"#,
    );
    let client = v2_wire_client(&untagged);
    let error = client
        .prompt("ses_1", "hi", &[], None, None, None, None)
        .await
        .unwrap_err();
    assert!(
        matches!(error, BridgeError::OpenCode(_)),
        "an untagged 404 must not trigger the recreate heal: {error:?}"
    );
}

/// Interrupt (`{interrupted:bool}`, false = idle no-op = success) and compact
/// (V2 declares an all-optional payload, so `{}` is sent; the `{data}` body is
/// ignored).
#[tokio::test]
async fn interrupt_accepts_the_idle_no_op_and_compact_sends_an_empty_payload() {
    let server = TestHttpServer::start().await;
    server.route(
        "POST",
        "/api/session/ses_1/interrupt",
        200,
        r#"{"interrupted":false}"#,
    );
    server.route(
        "POST",
        "/api/session/ses_1/compact",
        200,
        r#"{"data":{"id":"msg_c","type":"compaction","delivery":"steer"}}"#,
    );
    server.route(
        "POST",
        "/api/session/ses_gone/interrupt",
        404,
        r#"{"_tag":"SessionNotFoundError","message":"gone"}"#,
    );
    server.route(
        "POST",
        "/api/session/ses_gone/compact",
        404,
        r#"{"_tag":"SessionNotFoundError","message":"gone"}"#,
    );
    server.route(
        "POST",
        "/api/session/ses_proxy/compact",
        404,
        r#"{"message":"proxy has no such route"}"#,
    );
    let client = v2_wire_client(&server);

    client.interrupt("ses_1").await.unwrap();
    let interrupt = request_at(&server, 0);
    assert_eq!(interrupt.method, "POST");
    assert_eq!(interrupt.path, "/api/session/ses_1/interrupt");
    assert_eq!(interrupt.body, "", "interrupt carries no body");

    client.compact("ses_1").await.unwrap();
    let compact = request_at(&server, 1);
    assert_eq!(compact.method, "POST");
    assert_eq!(compact.path, "/api/session/ses_1/compact");
    assert_eq!(
        body_json(&compact),
        serde_json::json!({}),
        "V2's all-optional compact payload still needs a JSON body"
    );

    let error = client.interrupt("ses_gone").await.unwrap_err();
    assert!(matches!(error, BridgeError::SessionNotFound(_)), "{error:?}");
    // Compact gets the same mapping as the other write calls: a tagged 404 is
    // the missing session, an untagged one is not (a raw error_for_status()
    // would leak it as an Http 404, which reads as missing regardless of tag).
    let error = client.compact("ses_gone").await.unwrap_err();
    assert!(
        matches!(error, BridgeError::SessionNotFound(_)),
        "a tagged compact 404 is SessionNotFound: {error:?}"
    );
    let error = client.compact("ses_proxy").await.unwrap_err();
    assert!(
        matches!(error, BridgeError::OpenCode(_)),
        "an untagged compact 404 must not trigger the recreate heal: {error:?}"
    );
}

/// The durable-selection read: `GET /api/session/{id}`'s `{data}` envelope
/// carries the model ref (variant inside it) and the selected agent — the
/// state the effective-model ladder and the footer read.
#[tokio::test]
async fn session_selection_reads_the_model_ref_with_its_variant_and_the_agent() {
    let server = TestHttpServer::start().await;
    server.route(
        "GET",
        "/api/session/ses_1",
        200,
        serde_json::json!({
            "data": {
                "id": "ses_1",
                "agent": "live-agent",
                "model": {"id": "deepseek-v4-flash", "providerID": "opencode-go", "variant": "high"},
                "location": {"directory": "/work/cola"},
                "time": {"created": 1, "updated": 2},
            },
        })
        .to_string(),
    );
    let client = v2_wire_client(&server);

    let selection = client
        .session_selection("ses_1", None)
        .await
        .unwrap()
        .expect("a V2 session has a durable selection");
    let model = selection.model.expect("the model ref survives");
    assert_eq!(model.provider_id, "opencode-go");
    assert_eq!(model.id, "deepseek-v4-flash");
    assert_eq!(model.variant.as_deref(), Some("high"));
    assert_eq!(selection.agent.as_deref(), Some("live-agent"));

    // A selection without a model (a session the create left bare) still
    // reports, so the caller can tell "durable, nothing selected" from V1.
    server.route(
        "GET",
        "/api/session/ses_bare",
        200,
        serde_json::json!({"data": {"id": "ses_bare"}}).to_string(),
    );
    let bare = client.session_selection("ses_bare", None).await.unwrap().unwrap();
    assert!(bare.model.is_none());
    assert!(bare.agent.is_none());

    // V2 spells "no variant" as the literal `"default"` on the session read;
    // the neutral ref must not surface it as a real thinking level.
    server.route(
        "GET",
        "/api/session/ses_default",
        200,
        serde_json::json!({
            "data": {
                "id": "ses_default",
                "model": {"id": "deepseek-v4-flash", "providerID": "opencode-go", "variant": "default"},
            },
        })
        .to_string(),
    );
    let defaulted = client
        .session_selection("ses_default", None)
        .await
        .unwrap()
        .unwrap();
    assert!(
        defaulted.model.expect("a model").variant.is_none(),
        "the reserved `default` spelling is no variant"
    );
}

/// The session-scoped switches: V2's `POST /api/session/{id}/model` carries the
/// whole `Model.Ref` (variant inside it) and `/agent` the agent id, both
/// answering 204. A tagged 404 is session-not-found, not a bare proxy 404.
#[tokio::test]
async fn switch_model_and_agent_post_the_durable_selection_and_accept_204() {
    use crate::opencode::types::ModelInfo;

    let server = TestHttpServer::start().await;
    server.route("POST", "/api/session/ses_1/model", 204, "");
    server.route("POST", "/api/session/ses_1/agent", 204, "");
    server.route(
        "POST",
        "/api/session/ses_gone/model",
        404,
        serde_json::json!({"_tag": "SessionNotFoundError", "message": "gone"}).to_string(),
    );
    let client = v2_wire_client(&server);

    client
        .switch_session_model(
            "ses_1",
            &ModelInfo {
                id: "deepseek-v4-flash".into(),
                provider_id: "opencode-go".into(),
                variant: Some("high".into()),
            },
        )
        .await
        .unwrap();
    let model = last_request(&server);
    assert_eq!(model.method, "POST");
    assert_eq!(model.path, "/api/session/ses_1/model");
    assert_eq!(
        body_json(&model),
        serde_json::json!({
            "model": {"id": "deepseek-v4-flash", "providerID": "opencode-go", "variant": "high"},
        })
    );

    client.switch_session_agent("ses_1", "live-agent").await.unwrap();
    let agent = last_request(&server);
    assert_eq!(agent.path, "/api/session/ses_1/agent");
    assert_eq!(body_json(&agent), serde_json::json!({"agent": "live-agent"}));

    let error = client
        .switch_session_model(
            "ses_gone",
            &ModelInfo {
                id: "m".into(),
                provider_id: "p".into(),
                variant: None,
            },
        )
        .await
        .unwrap_err();
    assert!(
        matches!(error, BridgeError::SessionNotFound(ref id) if id == "ses_gone"),
        "a tagged 404 is the bridge's session-gone taxonomy: {error:?}"
    );
}

/// The agent catalog: `GET /api/agent`'s `{location, data}` envelope. The wire
/// `id` is the selectable identity (what a switch takes); the display `name`
/// is not cola's pick value, so the neutral view carries the id as its name.
#[tokio::test]
async fn list_agents_maps_the_wire_id_to_the_selectable_name() {
    let server = TestHttpServer::start().await;
    server.route(
        "GET",
        "/api/agent",
        200,
        serde_json::json!({
            "location": {"directory": "/work/cola"},
            "data": [
                {
                    "id": "build",
                    "name": "Build",
                    "description": "The default agent.",
                    "mode": "primary",
                    "hidden": false,
                    "permissions": [],
                },
                {
                    "id": "explore",
                    "name": "Explore",
                    "mode": "subagent",
                    "hidden": false,
                    "permissions": [],
                },
            ],
        })
        .to_string(),
    );
    let client = v2_wire_client(&server);

    let agents = client.list_agents().await;
    assert_eq!(agents.len(), 2);
    assert_eq!(agents[0].name, "build", "the id is the selectable value");
    assert_eq!(agents[0].description.as_deref(), Some("The default agent."));
    assert_eq!(agents[0].mode.as_deref(), Some("primary"));
    assert_eq!(agents[0].hidden, Some(false));
    assert_eq!(agents[1].name, "explore");
    assert_eq!(agents[1].mode.as_deref(), Some("subagent"));
    assert_eq!(
        crate::opencode::types::AgentInfo::default_agent(&agents).as_deref(),
        Some("build"),
        "the server's first primary visible agent is its default"
    );
}

/// The model catalog: `GET /api/model`'s `{location, data}` envelope groups
/// into the picker's `provider → models` view, with each model's declared
/// variants; the same read answers the footer's context-window lookup.
#[tokio::test]
async fn list_models_groups_the_catalog_and_reports_variants_and_context_windows() {
    let server = TestHttpServer::start().await;
    server.route(
        "GET",
        "/api/model",
        200,
        serde_json::json!({
            "location": {"directory": "/work/cola"},
            "data": [
                {
                    "id": "deepseek-v4-flash",
                    "providerID": "opencode-go",
                    "name": "DeepSeek V4 Flash",
                    "variants": [{"id": "low"}, {"id": "high"}],
                    "limit": {"context": 128000, "output": 4096},
                    "status": "active",
                    "enabled": true,
                    "capabilities": {"tools": true, "input": ["text"], "output": ["text"]},
                    "cost": [],
                    "time": {"released": 1},
                },
                {
                    "id": "gpt-4o",
                    "providerID": "opencode-go",
                    "variants": [],
                    "limit": {"context": 200000},
                    "status": "active",
                    "enabled": true,
                    "capabilities": {"tools": true, "input": ["text"], "output": ["text"]},
                    "cost": [],
                    "time": {"released": 2},
                },
                {
                    "id": "other-model",
                    "providerID": "openrouter",
                    "variants": [{"id": "medium"}],
                    "status": "active",
                    "enabled": true,
                    "capabilities": {"tools": true, "input": ["text"], "output": ["text"]},
                    "cost": [],
                    "time": {"released": 3},
                },
            ],
        })
        .to_string(),
    );
    let client = v2_wire_client(&server);

    let grouped = client.list_models().await;
    assert_eq!(grouped.len(), 2, "one group per provider");
    assert_eq!(grouped[0].provider, "opencode-go");
    assert_eq!(
        grouped[0]
            .models
            .iter()
            .map(|m| m.id.as_str())
            .collect::<Vec<_>>(),
        ["deepseek-v4-flash", "gpt-4o"]
    );
    assert_eq!(grouped[0].models[0].variants, ["low", "high"]);
    assert!(grouped[0].models[1].variants.is_empty());
    assert_eq!(grouped[1].provider, "openrouter");
    assert_eq!(grouped[1].models[0].variants, ["medium"]);

    assert_eq!(
        client
            .model_context_window("opencode-go", "deepseek-v4-flash")
            .await
            .unwrap(),
        Some(128_000)
    );
    assert_eq!(
        client
            .model_context_window("opencode-go", "gpt-4o")
            .await
            .unwrap(),
        Some(200_000)
    );
    // An unknown model, or a server without the route, degrades to None: the
    // footer omits the ratio rather than failing the read.
    assert_eq!(
        client.model_context_window("opencode-go", "nope").await.unwrap(),
        None
    );
    server.route("GET", "/api/model", 500, "boom");
    assert_eq!(
        client
            .model_context_window("opencode-go", "gpt-4o")
            .await
            .unwrap(),
        None
    );
}

/// The location-scoped pending-permission read: `GET /api/permission/request`
/// with V2's deepObject `location[directory]` query, the `{location, data}`
/// envelope, and the renamed request fields (`action`/`resources`/`save`).
#[tokio::test]
async fn list_permissions_reads_the_location_scoped_envelope_and_renamed_fields() {
    let server = TestHttpServer::start().await;
    server.route(
        "GET",
        "/api/permission/request",
        200,
        serde_json::json!({
            "location": {"directory": "/work/cola"},
            "data": [{
                "id": "per_1",
                "sessionID": "ses_1",
                "action": "shell",
                "resources": ["rm -rf *"],
                "save": ["rm *"],
                "metadata": {"command": "rm"},
            }],
        })
        .to_string(),
    );
    let client = v2_wire_client(&server);

    let permissions = client.list_permissions(Some("/work/cola")).await.unwrap();

    assert_eq!(permissions.len(), 1);
    assert_eq!(permissions[0].request_id, "per_1");
    assert_eq!(permissions[0].session_id.as_deref(), Some("ses_1"));
    assert_eq!(permissions[0].permission.as_deref(), Some("shell"));
    assert_eq!(permissions[0].patterns, vec!["rm -rf *"]);
    assert_eq!(permissions[0].always, vec!["rm *"]);

    let request = last_request(&server);
    assert_eq!(request.method, "GET");
    assert_eq!(request.path, "/api/permission/request");
    assert_eq!(
        request.query_param("location[directory]").as_deref(),
        Some("/work/cola"),
        "V2 scopes the list with the deepObject location query: {}",
        request.query
    );
}

/// The session-scoped permission reply: `POST
/// /api/session/{id}/permission/{requestID}/reply` with V2's `decision` body
/// and 204. A 404 (gone) and a 409 (settled) are both the benign "already
/// handled" taxonomy, never a failure card.
#[tokio::test]
async fn reply_permission_posts_the_session_scoped_decision_body() {
    let server = TestHttpServer::start().await;
    server.route("POST", "/api/session/ses_1/permission/per_1/reply", 204, "");
    server.route(
        "POST",
        "/api/session/ses_1/permission/per_gone/reply",
        404,
        serde_json::json!({"_tag": "PermissionNotFoundError", "requestID": "per_gone", "message": "gone"})
            .to_string(),
    );
    server.route(
        "POST",
        "/api/session/ses_1/permission/per_settled/reply",
        409,
        serde_json::json!({"_tag": "ConflictError", "message": "settled"}).to_string(),
    );
    let client = v2_wire_client(&server);

    client
        .reply_permission("ses_1", "per_1", "always", None)
        .await
        .unwrap();

    let request = last_request(&server);
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, "/api/session/ses_1/permission/per_1/reply");
    assert_eq!(body_json(&request), serde_json::json!({"decision": "always"}));

    for (id, label) in [("per_gone", "404"), ("per_settled", "409")] {
        let error = client
            .reply_permission("ses_1", id, "once", None)
            .await
            .unwrap_err();
        assert!(
            error.is_not_found(),
            "{label} must read as already handled: {error:?}"
        );
    }
}

/// The location-scoped pending-form read: one `GET /api/form` per known
/// directory, never one per session, with V2's typed `Form.Field` union decoded
/// into the neutral fields — keys, kinds, option values vs display labels.
#[tokio::test]
async fn list_questions_decodes_typed_form_fields_from_the_location_list() {
    let server = TestHttpServer::start().await;
    server.route(
        "GET",
        "/api/form",
        200,
        serde_json::json!({
            "location": {"directory": "/work/cola"},
            "data": [{
                "id": "frm_1",
                "sessionID": "ses_1",
                "title": "Questions",
                "fields": [
                    {
                        "key": "q0",
                        "title": "目录",
                        "description": "选哪个目录？",
                        "type": "string",
                        "required": true,
                        "custom": true,
                        "options": [
                            {"value": "/a", "label": "目录 A", "description": "第一个"},
                            {"value": "/b", "label": "目录 B"},
                        ],
                    },
                    {
                        "key": "q1",
                        "title": "水果",
                        "description": "选水果",
                        "type": "multiselect",
                        "options": [{"value": "apple", "label": "苹果"}],
                    },
                    {"key": "q2", "title": "确认", "description": "确定吗", "type": "boolean"},
                    {"key": "q3", "title": "数量", "description": "几个", "type": "integer"},
                    {"key": "q4", "title": "链接", "description": "打开", "type": "external", "url": "https://example.com/x"},
                ],
            }],
        })
        .to_string(),
    );
    let client = v2_wire_client(&server);

    let forms = client.list_questions(Some("/work/cola")).await.unwrap();

    assert_eq!(forms.len(), 1);
    let form = &forms[0];
    assert_eq!(form.id, "frm_1");
    assert_eq!(form.session_id, "ses_1");
    assert_eq!(form.title, "Questions");
    let fields = &form.questions;
    assert_eq!(fields.len(), 5);
    assert_eq!(fields[0].key, "q0");
    assert_eq!(fields[0].header, "目录");
    assert_eq!(fields[0].question, "选哪个目录？");
    assert!(fields[0].required);
    assert!(fields[0].custom_allowed());
    assert_eq!(fields[0].kind, crate::opencode::types::FormFieldKind::String);
    // The submitted value and the display label are distinct.
    assert_eq!(fields[0].options[0].answer_value(), "/a");
    assert_eq!(fields[0].options[0].label, "目录 A");
    assert_eq!(fields[0].display_values(&["/a".into()]), vec!["目录 A"]);
    assert!(fields[1].is_multi());
    assert_eq!(fields[2].kind, crate::opencode::types::FormFieldKind::Boolean);
    assert_eq!(fields[3].kind, crate::opencode::types::FormFieldKind::Integer);
    assert_eq!(fields[4].kind, crate::opencode::types::FormFieldKind::External);
    assert_eq!(fields[4].url.as_deref(), Some("https://example.com/x"));

    let request = last_request(&server);
    assert_eq!(request.method, "GET");
    assert_eq!(request.path, "/api/form");
    assert_eq!(
        request.query_param("location[directory]").as_deref(),
        Some("/work/cola")
    );
}

/// The session-scoped form reply: `POST /api/session/{id}/form/{formID}/reply`
/// with the keyed `{answer: {key: value}}` object whose values keep the field
/// types (string / array / boolean / number / integer — the integer stays a
/// JSON integer, never `3.0`). A settled form (409) is the same benign
/// "already handled" outcome as a missing one (404).
#[tokio::test]
async fn reply_question_posts_the_keyed_typed_answer_object() {
    use crate::opencode::types::{FormAnswer, FormValue};

    let server = TestHttpServer::start().await;
    server.route("POST", "/api/session/ses_1/form/frm_1/reply", 204, "");
    server.route(
        "POST",
        "/api/session/ses_1/form/frm_settled/reply",
        409,
        serde_json::json!({"_tag": "FormAlreadySettledError", "id": "frm_settled", "message": "settled"})
            .to_string(),
    );
    let client = v2_wire_client(&server);

    let answers = vec![
        FormAnswer {
            key: "q0".into(),
            value: Some(FormValue::Text("/a".into())),
        },
        FormAnswer {
            key: "q1".into(),
            value: Some(FormValue::List(vec!["apple".into(), "梨".into()])),
        },
        FormAnswer {
            key: "q2".into(),
            value: Some(FormValue::Bool(true)),
        },
        FormAnswer {
            key: "q3".into(),
            value: Some(FormValue::Number(3.5)),
        },
        FormAnswer {
            key: "q4".into(),
            value: Some(FormValue::Integer(4)),
        },
    ];
    client
        .reply_question("ses_1", "frm_1", &answers, Some("/work/cola"))
        .await
        .unwrap();

    let request = last_request(&server);
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, "/api/session/ses_1/form/frm_1/reply");
    let body = body_json(&request);
    assert_eq!(
        body,
        serde_json::json!({"answer": {
            "q0": "/a",
            "q1": ["apple", "梨"],
            "q2": true,
            "q3": 3.5,
            "q4": 4,
        }})
    );
    assert!(
        body["answer"]["q4"].is_i64(),
        "an integer field must reach the wire as a JSON integer: {body}"
    );
    assert!(
        body["answer"]["q3"].is_f64(),
        "a number field keeps its fractional value: {body}"
    );

    let error = client
        .reply_question("ses_1", "frm_settled", &answers, None)
        .await
        .unwrap_err();
    assert!(error.is_not_found(), "a settled form reads as handled: {error:?}");
}

/// Cancelling a V2 form is `DELETE /api/session/{id}/form/{formID}` (there is
/// no reject endpoint); a gone or settled form is the benign NotFound taxonomy.
#[tokio::test]
async fn reject_question_deletes_the_session_scoped_form() {
    let server = TestHttpServer::start().await;
    server.route("DELETE", "/api/session/ses_1/form/frm_1", 204, "");
    server.route(
        "DELETE",
        "/api/session/ses_1/form/frm_gone",
        404,
        serde_json::json!({"_tag": "FormNotFoundError", "id": "frm_gone", "message": "gone"}).to_string(),
    );
    server.route(
        "DELETE",
        "/api/session/ses_1/form/frm_settled",
        409,
        serde_json::json!({"_tag": "FormAlreadySettledError", "id": "frm_settled", "message": "settled"})
            .to_string(),
    );
    let client = v2_wire_client(&server);

    client.reject_question("ses_1", "frm_1", None).await.unwrap();

    let request = last_request(&server);
    assert_eq!(request.method, "DELETE");
    assert_eq!(request.path, "/api/session/ses_1/form/frm_1");
    assert_eq!(request.body, "", "cancel carries no body");

    for id in ["frm_gone", "frm_settled"] {
        let error = client.reject_question("ses_1", id, None).await.unwrap_err();
        assert!(error.is_not_found(), "{id} must read as handled: {error:?}");
    }
}
