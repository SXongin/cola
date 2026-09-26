//! The V2 half of the generation-parameterized conformance suite (spec #364,
//! "Testing Decisions"). The same neutral scenario bodies run against these
//! `/api` routes and V2's envelopes (a `{data}` wrapper, a body cursor, an
//! active map plus the transcript-derived retry).

use serde_json::json;

use crate::opencode::conformance::{SessionReadCase, SessionReadIds};
use crate::opencode::strategy::Generation;
use crate::test_http::{MockResponse, TestHttpServer};

/// The V2 session-read case: the `/api` read routes, with V2's response
/// shapes.
pub(crate) fn session_read_case() -> SessionReadCase {
    SessionReadCase {
        generation: Generation::V2,
        mount,
    }
}

fn mount(server: &TestHttpServer) -> SessionReadIds {
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
                            "id": "ses_new",
                            "title": "新",
                            "agent": "build",
                            "model": {"id": "deepseek-v4-flash", "providerID": "opencode-go"},
                            "location": {"directory": "/work/cola"},
                            "time": {"created": 1, "updated": 3},
                        },
                        {
                            "id": "ses_child",
                            "title": "子会话",
                            "parentID": "ses_parent",
                            "location": {"directory": "/work/cola"},
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
                        "id": "ses_old",
                        "title": "旧",
                        "location": {"directory": "/work/other"},
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
        "/api/session/ses_child",
        200,
        json!({
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
    server.route(
        "GET",
        "/api/session/active",
        200,
        json!({"data": {"ses_busy": {"type": "running"}}}).to_string(),
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
        "/api/session/ses_retry/message",
        200,
        json!({
            "data": [{"type": "assistant", "retry": {"attempt": 1, "at": 5000, "error": {"message": "boom"}}}],
            "cursor": {},
        })
        .to_string(),
    );

    SessionReadIds {
        newest: "ses_new",
        child: "ses_child",
        oldest: "ses_old",
        parent: "ses_parent",
        directory: "/work/cola",
        other_directory: "/work/other",
        title: "新",
        child_title: "子会话",
        provider: "opencode-go",
        model: "deepseek-v4-flash",
        idle: "ses_idle",
        busy: "ses_busy",
        retrying: "ses_retry",
    }
}
