//! The V1 half of the generation-parameterized conformance suite (spec #364,
//! "Testing Decisions"). This file lives inside the V1 strategy's allow-listed
//! path, so V1 route literals stay exactly where retirement deletes them. The
//! payload values come from the shared
//! [`SessionReadFixture`](crate::opencode::conformance::SessionReadFixture).

use serde_json::{Map, Value, json};

use crate::opencode::conformance::{SessionReadCase, SessionReadFixture};
use crate::opencode::strategy::Generation;
use crate::test_http::{MockResponse, TestHttpServer};

/// The V1 session-read case: the unprefixed read routes, with V1's response
/// shapes (a bare list array + a header cursor, a bare session object, a status
/// map).
pub(crate) fn session_read_case() -> SessionReadCase {
    SessionReadCase {
        generation: Generation::V1,
        mount,
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
