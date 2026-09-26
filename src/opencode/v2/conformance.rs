//! The V2 half of the generation-parameterized conformance suite (spec #364,
//! "Testing Decisions"). The same neutral scenario bodies run against these
//! `/api` routes and V2's envelopes (a `{data}` wrapper, a body cursor, an
//! active map plus the transcript-derived retry). The payload values come from
//! the shared
//! [`SessionReadFixture`](crate::opencode::conformance::SessionReadFixture).

use serde_json::{Map, json};

use crate::opencode::conformance::{SessionReadCase, SessionReadFixture};
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
