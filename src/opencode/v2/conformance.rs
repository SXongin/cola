//! The V2 half of the generation-parameterized conformance suite (spec #364,
//! "Testing Decisions"). The same neutral scenario bodies run against these
//! `/api` routes and V2's envelopes (a `{data}` wrapper, a body cursor, an
//! active map whose running sessions get their Retry state from the newest
//! assistant message's `retry` field). The payload values come from the shared
//! [`SessionReadFixture`](crate::opencode::conformance::SessionReadFixture).

use serde_json::{Map, json};

use crate::opencode::conformance::{
    PromptFixture, RequestFixture, SessionCase, SessionReadFixture, SkillListFixture, TranscriptFixture,
};
use crate::opencode::strategy::Generation;
use crate::test_http::{DynamicResponse, MockResponse, TestHttpServer};

/// The V2 conformance case: the `/api` routes, with V2's response shapes
/// (`{data}` envelopes, a body cursor, the active map, and the admit + wait +
/// transcript write path).
pub(crate) fn case() -> SessionCase {
    SessionCase {
        generation: Generation::V2,
        keeps_session_selection: true,
        reuse_continues_an_admitted_turn: false,
        resume_supported: true,
        mount,
        mount_transcript,
        mount_recorded_transcript,
        mount_prompt,
        mount_requests,
        mount_selection,
        mount_skills,
    }
}

/// The V2 skill read: `GET /api/skill`, a `{location, data}` envelope of
/// `Skill.Info`. The wire `id` is the picker's callback value and the
/// structured prompt attach; `description` is optional; `autoinvoke: false`
/// marks a hidden skill, which the unfiltered list still returns.
fn mount_skills(server: &TestHttpServer, fixture: &SkillListFixture) {
    server.route(
        "GET",
        "/api/skill",
        200,
        json!({
            "location": {"directory": "/work/cola"},
            "data": [
                {
                    "id": fixture.visible_id,
                    "name": fixture.visible_name,
                    "description": fixture.visible_description,
                    "autoinvoke": true,
                    "path": "/work/skills/implement-spec/SKILL.md",
                    "content": "body",
                },
                {
                    "id": fixture.bare_name,
                    "name": fixture.bare_name,
                    "path": "/work/skills/description-less/SKILL.md",
                    "content": "body",
                },
                {
                    "id": fixture.hidden_id,
                    "name": fixture.hidden_name,
                    "description": "Hidden from the model.",
                    "autoinvoke": false,
                    "path": "/work/skills/hidden-tool/SKILL.md",
                    "content": "body",
                },
            ],
        })
        .to_string(),
    );
}

/// Mount a stateful durable selection: `GET /api/session/{id}` serves the
/// stored `Session.Info` and the two switch routes mutate it, so a switch is
/// observable in the following selection read — V2's native semantics.
fn mount_selection(server: &TestHttpServer, fixture: &SessionReadFixture) {
    use std::sync::{Arc, Mutex};

    let state = Arc::new(Mutex::new(json!({
        "id": fixture.newest,
        "agent": fixture.agent,
        "model": {"id": fixture.model, "providerID": fixture.provider},
        "location": {"directory": fixture.directory},
        "time": {"created": 1, "updated": 2},
    })));

    let get_state = Arc::clone(&state);
    server.route_dynamic(
        "GET",
        &format!("/api/session/{}", fixture.newest),
        move |_request| {
            let data = get_state.lock().expect("selection state lock").clone();
            DynamicResponse::new(200, "application/json", json!({"data": data}).to_string())
        },
    );

    let model_state = Arc::clone(&state);
    server.route_dynamic(
        "POST",
        &format!("/api/session/{}/model", fixture.newest),
        move |request| {
            let body: serde_json::Value =
                serde_json::from_str(&request.body).expect("a model switch body is JSON");
            model_state
                .lock()
                .expect("selection state lock")
                .as_object_mut()
                .expect("session object")
                .insert("model".into(), body["model"].clone());
            DynamicResponse::new(204, "application/json", "")
        },
    );

    let agent_state = Arc::clone(&state);
    server.route_dynamic(
        "POST",
        &format!("/api/session/{}/agent", fixture.newest),
        move |request| {
            let body: serde_json::Value =
                serde_json::from_str(&request.body).expect("an agent switch body is JSON");
            agent_state
                .lock()
                .expect("selection state lock")
                .as_object_mut()
                .expect("session object")
                .insert("agent".into(), body["agent"].clone());
            DynamicResponse::new(204, "application/json", "")
        },
    );
}

fn mount(server: &TestHttpServer, fixture: &SessionReadFixture) {
    // Two pages plus the terminating empty page: V2 emits `cursor.next` for
    // every non-empty page, so the follow-up always sees an end-of-list page.
    server.route_sequence(
        "GET",
        "/api/session",
        vec![
            MockResponse::json(
                json!({
                    "data": [
                        {
                            "id": fixture.newest,
                            "title": fixture.title,
                            "agent": fixture.agent,
                            "model": {"id": fixture.model, "providerID": fixture.provider},
                            "location": {"directory": fixture.directory},
                            "time": {"created": 1, "updated": 3},
                        },
                        {
                            "id": fixture.child,
                            "title": fixture.child_title,
                            "parentID": fixture.parent,
                            "location": {"directory": fixture.directory},
                            "time": {"created": 1, "updated": 2},
                        }
                    ],
                    "cursor": {"next": "c1"},
                })
                .to_string(),
            ),
            MockResponse::json(
                json!({
                    "data": [{
                        "id": fixture.oldest,
                        "title": fixture.other_title,
                        "location": {"directory": fixture.other_directory},
                        "time": {"created": 1, "updated": 1},
                    }],
                    "cursor": {"next": "c2"},
                })
                .to_string(),
            ),
            MockResponse::json(json!({"data": [], "cursor": {}}).to_string()),
        ],
    );
    server.route(
        "GET",
        &format!("/api/session/{}", fixture.child),
        200,
        json!({
            "data": {
                "id": fixture.child,
                "parentID": fixture.parent,
                "title": fixture.child_title,
                "model": {"providerID": fixture.provider, "id": fixture.model},
                "location": {"directory": fixture.directory},
                "time": {"created": 1, "updated": 2},
            },
        })
        .to_string(),
    );
    // The active map is keyed by session id, so it is built rather than
    // literal-keyed. Both the running and the retrying fixture sessions are
    // active: a scheduled retry keeps the drain active (that is why the retry
    // read is only paid for active sessions).
    let mut active = Map::new();
    active.insert(fixture.busy.to_string(), json!({"type": "running"}));
    active.insert(fixture.retrying.to_string(), json!({"type": "running"}));
    server.route(
        "GET",
        "/api/session/active",
        200,
        json!({"data": active}).to_string(),
    );
    // The retry derivation's minimal assistant read: clear for everyone except
    // the retrying session, whose newest assistant carries the field.
    server.route(
        "GET",
        "/api/session/",
        200,
        json!({"data": [{"type": "assistant", "retry": null}], "cursor": {}}).to_string(),
    );
    server.route(
        "GET",
        &format!("/api/session/{}/message", fixture.retrying),
        200,
        json!({
            "data": [{"type": "assistant", "retry": {"attempt": 1, "at": 5000, "error": {"message": "boom"}}}],
            "cursor": {},
        })
        .to_string(),
    );
}

/// Mount the V2 transcript read: the `{data, cursor}` envelope whose items are
/// a tagged union. The shared [`TranscriptFixture`] publishes the same neutral
/// facts as V1's mount through V2's spellings: `type`/`time`, `content[]` with
/// `text`/`reasoning`/`tool`, the tool's own `id`, and the `finish` field whose
/// reason declares completion. The body ends before any cursor (`cursor: {}`),
/// so the neutral scenario does not depend on pagination; the V2 wire suite
/// and the recorded corpus pin the cursor follow.
fn mount_transcript(server: &TestHttpServer, fixture: &TranscriptFixture) {
    server.route(
        "GET",
        &format!("/api/session/{}/message", fixture.session),
        200,
        json!({
            "data": [
                {
                    "id": fixture.previous_id,
                    "type": "assistant",
                    "time": {"created": fixture.previous_created_ms, "completed": fixture.previous_completed_ms},
                    "model": {"id": fixture.model, "providerID": fixture.provider},
                    "content": [{"type": "text", "text": fixture.previous_text}],
                    "finish": "stop",
                },
                {
                    "id": fixture.inflight_id,
                    "type": "assistant",
                    "time": {"created": fixture.inflight_created_ms},
                    "model": {"id": fixture.model, "providerID": fixture.provider},
                    "content": [
                        {"type": "reasoning", "text": fixture.inflight_reasoning,
                         "time": {"created": fixture.inflight_created_ms}},
                        {"type": "tool", "id": fixture.inflight_call_id, "name": "shell",
                         "state": {"status": "running", "input": {"command": "sleep 10"}, "metadata": {}},
                         "time": {"created": fixture.inflight_created_ms}},
                    ],
                },
                {
                    "id": fixture.user_id,
                    "type": "user",
                    "time": {"created": fixture.user_created_ms},
                    "text": fixture.user_text,
                },
                {
                    "id": fixture.assistant_id,
                    "type": "assistant",
                    "time": {"created": fixture.assistant_created_ms, "completed": fixture.assistant_completed_ms},
                    "model": {"id": fixture.model, "providerID": fixture.provider},
                    "tokens": {
                        "input": fixture.input_tokens,
                        "output": fixture.output_tokens,
                        "reasoning": fixture.reasoning_tokens,
                        "cache": {"read": fixture.cache_read_tokens, "write": fixture.cache_write_tokens},
                    },
                    "content": [
                        {"type": "reasoning", "text": fixture.assistant_reasoning,
                         "time": {"created": fixture.assistant_created_ms + 10}},
                        {"type": "tool", "id": fixture.tool_call_id, "name": "shell",
                         "state": {"status": "completed",
                                   "input": {"command": fixture.tool_command},
                                   "content": [{"type": "text", "text": fixture.tool_output}],
                                   "metadata": {"exit": 0}},
                         "time": {"created": fixture.assistant_created_ms + 20,
                                  "completed": fixture.assistant_completed_ms}},
                        {"type": "text", "text": fixture.assistant_text},
                    ],
                    "finish": "stop",
                },
            ],
            "cursor": {},
        })
        .to_string(),
    );
}

/// Serve a recorded V2 transcript response body verbatim, then the terminating
/// empty page: a real recording carries `cursor.next` even on its last data
/// page (the server mints one for every non-empty page), so the follow must see
/// an end-of-list page rather than loop on the recording.
fn mount_recorded_transcript(server: &TestHttpServer, session_id: &str, body: &str) {
    server.route_sequence(
        "GET",
        &format!("/api/session/{session_id}/message"),
        vec![
            MockResponse::json(body),
            MockResponse::json(json!({"data": [], "cursor": {}}).to_string()),
        ],
    );
}

/// Mount V2's prompt submit: the durable admit (`POST
/// /api/session/{id}/prompt`, 200 `{data: Session.Inbox.User}`) and the
/// transcript read serving the turn it scheduled — the observation source the
/// async-native Turn drives completion from. The shared [`PromptFixture`] names
/// the values; only V2's spellings live here.
fn mount_prompt(server: &TestHttpServer, fixture: &PromptFixture) {
    server.route(
        "POST",
        &format!("/api/session/{}/prompt", fixture.session),
        200,
        json!({
            "data": {
                "id": fixture.message_id,
                "sessionID": fixture.session,
                "type": "user",
                "delivery": "steer",
                "time": {"created": 1},
                "payload": {"text": fixture.text},
            },
        })
        .to_string(),
    );
    server.route(
        "GET",
        &format!("/api/session/{}/message", fixture.session),
        200,
        json!({
            "data": [
                {
                    "id": fixture.message_id,
                    "type": "user",
                    "time": {"created": 1000},
                    "text": fixture.text,
                },
                {
                    "id": fixture.answer_id,
                    "type": "assistant",
                    "time": {"created": 1010, "completed": 1100},
                    "model": {"id": "deepseek-v4-flash", "providerID": "opencode-go"},
                    "content": [{"type": "text", "text": fixture.answer_text}],
                    "finish": "stop",
                },
            ],
            "cursor": {},
        })
        .to_string(),
    );
}

/// Mount V2's location-scoped permission/form routes: the `{location, data}`
/// envelopes, the session-scoped reply/cancel paths, and the renamed
/// permission fields / typed form fields. The option's submitted `value` and
/// its display `label` are deliberately different — the neutral view must
/// carry both.
fn mount_requests(server: &TestHttpServer, fixture: &RequestFixture) {
    server.route(
        "GET",
        "/api/permission/request",
        200,
        json!({
            "location": {"directory": fixture.directory},
            "data": [{
                "id": fixture.permission_id,
                "sessionID": fixture.session,
                "action": fixture.action,
                "resources": [fixture.resource],
                "save": [],
                "metadata": {},
            }],
        })
        .to_string(),
    );
    server.route(
        "POST",
        &format!(
            "/api/session/{}/permission/{}/reply",
            fixture.session, fixture.permission_id
        ),
        204,
        "",
    );
    server.route(
        "GET",
        "/api/form",
        200,
        json!({
            "location": {"directory": fixture.directory},
            "data": [{
                "id": fixture.form_id,
                "sessionID": fixture.session,
                "title": fixture.form_title,
                "fields": [{
                    "key": fixture.field_key,
                    "title": fixture.field_title,
                    "description": fixture.field_question,
                    "type": "string",
                    "options": [{
                        "value": fixture.option_value,
                        "label": fixture.option_label,
                    }],
                    "custom": false,
                }],
            }],
        })
        .to_string(),
    );
    server.route(
        "POST",
        &format!("/api/session/{}/form/{}/reply", fixture.session, fixture.form_id),
        204,
        "",
    );
    server.route(
        "DELETE",
        &format!("/api/session/{}/form/{}", fixture.session, fixture.form_id),
        204,
        "",
    );
}
