//! The V1 half of the generation-parameterized conformance suite (spec #364,
//! "Testing Decisions"). This file lives inside the V1 strategy's allow-listed
//! path, so V1 route literals stay exactly where retirement deletes them.

use serde_json::json;

use crate::opencode::conformance::{SessionReadCase, SessionReadIds};
use crate::opencode::strategy::Generation;
use crate::test_http::{MockResponse, TestHttpServer};

/// The V1 session-read case: the unprefixed read routes, with V1's response
/// shapes (a bare list array + `x-next-cursor`, a bare session object, a status
/// map).
pub(crate) fn session_read_case() -> SessionReadCase {
    SessionReadCase {
        generation: Generation::V1,
        mount,
    }
}

fn mount(server: &TestHttpServer) -> SessionReadIds {
    // Two pages, most recently updated first, with the cursor in the header —
    // the second page carries no header (the end).
    server.route_sequence(
        "GET",
        "/experimental/session",
        vec![
            MockResponse::json(
                json!([
                    {
                        "id": "ses_new",
                        "title": "新",
                        "directory": "/work/cola",
                        "agent": "build",
                        "model": {"id": "deepseek-v4-flash", "providerID": "opencode-go"},
                        "time": {"created": 1, "updated": 3},
                    },
                    {
                        "id": "ses_child",
                        "title": "子会话",
                        "directory": "/work/cola",
                        "parentID": "ses_parent",
                        "time": {"created": 1, "updated": 2},
                    }
                ])
                .to_string(),
            )
            .header("x-next-cursor", "1700000000002"),
            MockResponse::json(
                json!([{
                    "id": "ses_old",
                    "title": "旧",
                    "directory": "/work/other",
                    "time": {"created": 1, "updated": 1},
                }])
                .to_string(),
            ),
        ],
    );
    server.route(
        "GET",
        "/session/ses_child",
        200,
        json!({
            "id": "ses_child",
            "parentID": "ses_parent",
            "title": "子会话",
            "model": {"providerID": "opencode-go", "id": "deepseek-v4-flash"},
        })
        .to_string(),
    );
    server.route(
        "GET",
        "/session/status",
        200,
        json!({
            "ses_idle": {"type": "idle"},
            "ses_busy": {"type": "busy"},
            "ses_retry": {"type": "retry", "attempt": 1, "message": "boom", "next": 5000},
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
