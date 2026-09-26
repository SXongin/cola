//! The V1 half of the generation-parameterized conformance suite (spec #364,
//! "Testing Decisions"). This file lives inside the V1 strategy's allow-listed
//! path, so V1 route literals stay exactly where retirement deletes them. The
//! payload values come from the shared
//! [`SessionReadFixture`](crate::opencode::conformance::SessionReadFixture).

use serde_json::{Map, Value, json};

use crate::opencode::conformance::{PromptFixture, SessionCase, SessionReadFixture, TranscriptFixture};
use crate::opencode::strategy::Generation;
use crate::test_http::{MockResponse, TestHttpServer};

/// The V1 conformance case: the unprefixed routes, with V1's response shapes
/// (a bare list array + a header cursor, a bare session object, a status map,
/// the blocking prompt response).
pub(crate) fn case() -> SessionCase {
    SessionCase {
        generation: Generation::V1,
        mount,
        mount_transcript,
        mount_recorded_transcript,
        mount_prompt,
    }
}

fn mount(server: &TestHttpServer, fixture: &SessionReadFixture) {
    // Two pages, most recently updated first, with the cursor in the header —
    // the second page carries no header (the end).
    server.route_sequence(
        "GET",
        "/experimental/session",
        vec![
            MockResponse::json(
                json!([
                    {
                        "id": fixture.newest,
                        "title": fixture.title,
                        "directory": fixture.directory,
                        "agent": fixture.agent,
                        "model": {"id": fixture.model, "providerID": fixture.provider},
                        "time": {"created": 1, "updated": 3},
                    },
                    {
                        "id": fixture.child,
                        "title": fixture.child_title,
                        "directory": fixture.directory,
                        "parentID": fixture.parent,
                        "time": {"created": 1, "updated": 2},
                    }
                ])
                .to_string(),
            )
            .header("x-next-cursor", "1700000000002"),
            MockResponse::json(
                json!([{
                    "id": fixture.oldest,
                    "title": fixture.other_title,
                    "directory": fixture.other_directory,
                    "time": {"created": 1, "updated": 1},
                }])
                .to_string(),
            ),
        ],
    );
    server.route(
        "GET",
        &format!("/session/{}", fixture.child),
        200,
        json!({
            "id": fixture.child,
            "parentID": fixture.parent,
            "title": fixture.child_title,
            "model": {"providerID": fixture.provider, "id": fixture.model},
        })
        .to_string(),
    );
    // The status map is keyed by session id, so it is built rather than
    // literal-keyed.
    let mut statuses = Map::new();
    statuses.insert(fixture.idle.to_string(), json!({"type": "idle"}));
    statuses.insert(fixture.busy.to_string(), json!({"type": "busy"}));
    statuses.insert(
        fixture.retrying.to_string(),
        json!({"type": "retry", "attempt": 1, "message": "boom", "next": 5000}),
    );
    server.route("GET", "/session/status", 200, Value::Object(statuses).to_string());
}

/// Mount the V1 transcript read: a bare `[{info, parts}]` array, in server
/// order. The shared [`TranscriptFixture`] publishes the same neutral facts as
/// V2's mount through V1's spellings: `info.role`/`info.time`, `parts` with
/// `text`/`reasoning`/`tool`, `callID`/`state`, and the `step-finish` part
/// whose reason declares completion.
fn mount_transcript(server: &TestHttpServer, fixture: &TranscriptFixture) {
    server.route(
        "GET",
        &format!("/session/{}/message", fixture.session),
        200,
        json!([
            {
                "info": {
                    "id": fixture.previous_id,
                    "role": "assistant",
                    "time": {"created": fixture.previous_created_ms, "completed": fixture.previous_completed_ms},
                    "providerID": fixture.provider,
                    "modelID": fixture.model,
                },
                "parts": [
                    {"type": "text", "text": fixture.previous_text},
                    {"type": "step-finish", "reason": "stop"},
                ],
            },
            {
                "info": {
                    "id": fixture.inflight_id,
                    "role": "assistant",
                    "time": {"created": fixture.inflight_created_ms},
                    "providerID": fixture.provider,
                    "modelID": fixture.model,
                },
                "parts": [
                    {"type": "reasoning", "text": fixture.inflight_reasoning,
                     "time": {"start": fixture.inflight_created_ms}},
                    {"type": "tool", "tool": "bash", "callID": fixture.inflight_call_id,
                     "state": {"status": "running", "input": {"command": "sleep 10"},
                               "time": {"start": fixture.inflight_created_ms}}},
                ],
            },
            {
                "info": {
                    "id": fixture.user_id,
                    "role": "user",
                    "time": {"created": fixture.user_created_ms},
                },
                "parts": [{"type": "text", "text": fixture.user_text}],
            },
            {
                "info": {
                    "id": fixture.assistant_id,
                    "role": "assistant",
                    "time": {"created": fixture.assistant_created_ms, "completed": fixture.assistant_completed_ms},
                    "providerID": fixture.provider,
                    "modelID": fixture.model,
                    "tokens": {
                        "input": fixture.input_tokens,
                        "output": fixture.output_tokens,
                        "total": fixture.total_tokens,
                        "cache": {"read": fixture.cache_read_tokens, "write": fixture.cache_write_tokens},
                    },
                },
                "parts": [
                    {"type": "step-start"},
                    {"type": "reasoning", "text": fixture.assistant_reasoning,
                     "time": {"start": fixture.assistant_created_ms + 10}},
                    {"type": "tool", "tool": "bash", "callID": fixture.tool_call_id,
                     "state": {"status": "completed",
                               "input": {"command": fixture.tool_command},
                               "output": fixture.tool_output,
                               "time": {"start": fixture.assistant_created_ms + 20,
                                        "end": fixture.assistant_completed_ms}}},
                    {"type": "text", "text": fixture.assistant_text},
                    {"type": "step-finish", "reason": "stop"},
                ],
            },
        ])
        .to_string(),
    );
}

/// Serve a recorded V1 transcript response body (a bare array) verbatim.
fn mount_recorded_transcript(server: &TestHttpServer, session_id: &str, body: &str) {
    server.route("GET", &format!("/session/{session_id}/message"), 200, body);
}

/// Mount the V1 blocking prompt: `POST /session/{id}/message` answers the
/// assistant message inline (`{info, parts}`), exactly as the live server does
/// when the turn finishes. The answer's `parts` carry the same neutral values
/// the shared [`PromptFixture`] names.
fn mount_prompt(server: &TestHttpServer, fixture: &PromptFixture) {
    server.route(
        "POST",
        &format!("/session/{}/message", fixture.session),
        200,
        json!({
            "info": {
                "id": fixture.answer_id,
                "parentID": fixture.message_id,
                "role": "assistant",
                "time": {"created": 1010, "completed": 1100},
                "providerID": "opencode-go",
                "modelID": "deepseek-v4-flash",
            },
            "parts": [
                {"type": "text", "text": fixture.answer_text},
                {"type": "step-finish", "reason": "stop"},
            ],
        })
        .to_string(),
    );
}
