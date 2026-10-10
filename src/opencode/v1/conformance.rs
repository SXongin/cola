//! The V1 half of the generation-parameterized conformance suite (spec #364,
//! "Testing Decisions"). This file lives inside the V1 strategy's allow-listed
//! path, so V1 route literals stay exactly where retirement deletes them. The
//! payload values come from the shared
//! [`SessionReadFixture`](crate::opencode::conformance::SessionReadFixture).

use serde_json::{Map, Value, json};

use crate::opencode::conformance::{
    PromptFixture, RequestFixture, SessionCase, SessionReadFixture, SkillListFixture, TranscriptFixture,
};
use crate::opencode::strategy::Generation;
use crate::test_http::{MockResponse, TestHttpServer};

/// The V1 conformance case: the unprefixed routes, with V1's response shapes
/// (a bare list array + a header cursor, a bare session object, a status map,
/// and the fire-and-forget prompt submit).
pub(crate) fn case() -> SessionCase {
    SessionCase {
        generation: Generation::V1,
        keeps_session_selection: false,
        reuse_continues_an_admitted_turn: true,
        resume_supported: false,
        mount,
        mount_transcript,
        mount_recorded_transcript,
        mount_prompt,
        mount_requests,
        mount_selection,
        mount_skills,
    }
}

/// V1 mounts nothing: it has no session switch routes, so the conformance
/// scenario's switches are no-ops and must stay off the wire.
fn mount_selection(_server: &TestHttpServer, _fixture: &SessionReadFixture) {}

/// The V1 skill read: `GET /skill`, a bare `Skill.Info` array (no envelope).
/// V1 keys skills by name — there is no id — so each entry carries only
/// `name` + optional `description` (+ `location`/`content`, ignored).
fn mount_skills(server: &TestHttpServer, fixture: &SkillListFixture) {
    server.route(
        "GET",
        "/skill",
        200,
        json!([
            {
                "name": fixture.visible_name,
                "description": fixture.visible_description,
                "location": "/work/skills/implement-spec/SKILL.md",
                "content": fixture.visible_content,
            },
            {
                "name": fixture.bare_name,
                "location": "/work/skills/description-less/SKILL.md",
                "content": fixture.visible_content,
            },
            {
                "name": fixture.hidden_name,
                "location": "/work/skills/hidden-tool/SKILL.md",
                "content": fixture.visible_content,
            },
        ])
        .to_string(),
    );
}

/// V1's skill read is best-effort (spec #652, tickets #655/#656): a body that
/// is not the expected array (a malformed/unreadable read) yields an EMPTY
/// list, never a guessed or partial one — the picker degrades to its no-skills
/// state.
#[tokio::test]
async fn a_malformed_skill_body_yields_an_empty_list() {
    let server = TestHttpServer::start().await;
    server.route("GET", "/skill", 200, "{}");
    let backend = case().backend(&server);
    assert!(
        backend.list_skills(Some("/work/cola")).await.is_empty(),
        "a non-array body must read as no skills"
    );
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

/// Mount the V1 prompt submit: `POST /session/{id}/prompt_async` answers 204
/// (the native fire-and-forget route), and the transcript read serves the turn
/// it scheduled — the observation source the async-native Turn drives
/// completion from. The answer's parts carry the same neutral values the
/// shared [`PromptFixture`] names.
fn mount_prompt(server: &TestHttpServer, fixture: &PromptFixture) {
    server.route(
        "POST",
        &format!("/session/{}/prompt_async", fixture.session),
        204,
        "",
    );
    server.route(
        "GET",
        &format!("/session/{}/message", fixture.session),
        200,
        json!([
            {
                "info": {
                    "id": fixture.message_id,
                    "role": "user",
                    "time": {"created": 1000},
                },
                "parts": [{"type": "text", "text": fixture.text}],
            },
            {
                "info": {
                    "id": fixture.answer_id,
                    "role": "assistant",
                    "time": {"created": 1010, "completed": 1100},
                    "providerID": "opencode-go",
                    "modelID": "deepseek-v4-flash",
                },
                "parts": [
                    {"type": "text", "text": fixture.answer_text},
                    {"type": "step-finish", "reason": "stop"},
                ],
            },
        ])
        .to_string(),
    );
}

/// Mount V1's global permission/question routes: a bare list array, the
/// `{"reply"}` / `{"answers"}` reply bodies, and the reject endpoint. V1 has no
/// field keys or typed options — the question's label is its submitted value.
fn mount_requests(server: &TestHttpServer, fixture: &RequestFixture) {
    server.route(
        "GET",
        "/permission",
        200,
        json!([{
            "id": fixture.permission_id,
            "sessionID": fixture.session,
            "permission": fixture.action,
            "patterns": [fixture.resource],
            "always": [],
            "metadata": {},
        }])
        .to_string(),
    );
    server.route(
        "POST",
        &format!("/permission/{}/reply", fixture.permission_id),
        200,
        "true",
    );
    server.route(
        "GET",
        "/question",
        200,
        json!([{
            "id": fixture.form_id,
            "sessionID": fixture.session,
            "questions": [{
                "question": fixture.field_question,
                "header": fixture.field_title,
                "options": [
                    {"label": fixture.option_value, "description": ""},
                ],
                "multiple": false,
                "custom": false,
            }],
        }])
        .to_string(),
    );
    server.route(
        "POST",
        &format!("/question/{}/reply", fixture.form_id),
        200,
        "true",
    );
    server.route(
        "POST",
        &format!("/question/{}/reject", fixture.form_id),
        200,
        "true",
    );
}
