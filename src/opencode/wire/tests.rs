//! The recorded-response corpus tests: every committed fixture decodes through
//! the production path — the real adapter, the generation's strategy, its
//! decoder — mounted verbatim on that generation's fake route. These pin the
//! upstream shapes the hermetic conformance payloads only approximate, and the
//! turn projections the Session Snapshot tail, the external-message sync and
//! the follow renderers consume.

use crate::backend::{MessageRole, Part, SessionTranscript, ToolStatus};
use crate::opencode::conformance::SessionCase;
use crate::opencode::strategy::Generation;
use crate::test_http::TestHttpServer;

use super::{RecordedResponse, v1_transcript_turn, v2_transcript_inflight, v2_transcript_turn};

/// The session id the recorded bodies are served under: the recording's own id
/// was sanitized, and the route is per-test, so the value only has to be one
/// both the mount and the read agree on.
const RECORDED_SESSION: &str = "ses_recording";

/// The text-free assertions on a decoded completed turn, shared by both
/// generations (the tool's shell name is the generation's own, so it is not
/// asserted here).
fn assert_completed_turn(transcript: &SessionTranscript) {
    let user = transcript
        .newest_user()
        .unwrap_or_else(|| panic!("a recorded turn must have a user message: {transcript:#?}"));
    assert_eq!(user.role, MessageRole::User);
    assert_eq!(user.text(), "run the live harness command");
    assert!(
        crate::opencode::parsing::is_cola_message_id(user.id.as_str()),
        "the recorded user message is cola-authored: {user:#?}"
    );

    let anchor = user
        .anchor()
        .expect("the recorded user message has a server time");
    let turn = transcript.turn_for_user(&anchor);
    assert!(
        turn.complete,
        "the recorded turn ends on a terminal finish: {turn:#?}"
    );
    assert_eq!(
        turn.messages.len(),
        2,
        "the tool step and the closing step both belong to the turn"
    );
    assert!(
        turn.messages
            .iter()
            .all(|message| message.time.is_some_and(|time| time.completed.is_some())),
        "every recorded assistant step is settled"
    );

    // Each scripted marker appears exactly once: the recorded payload carries
    // one content item per fact, never a duplicate.
    let texts: Vec<&str> = transcript
        .messages
        .iter()
        .flat_map(|message| message.parts.iter())
        .filter_map(|part| match part {
            Part::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect();
    for expected in ["run the live harness command", "live-harness-final-text"] {
        assert_eq!(
            texts.iter().filter(|text| **text == expected).count(),
            1,
            "`{expected}` must appear exactly once: {texts:?}"
        );
    }
    let reasoning = transcript
        .messages
        .iter()
        .flat_map(|message| message.parts.iter())
        .filter(|part| matches!(part, Part::Reasoning(_)))
        .count();
    assert_eq!(
        reasoning, 1,
        "the recorded reasoning appears once: {transcript:#?}"
    );

    // The settled shell call: its scripted command and its output text.
    let tool = transcript
        .messages
        .iter()
        .flat_map(|message| message.parts.iter())
        .find_map(|part| match part {
            Part::Tool(call) => Some(call),
            _ => None,
        })
        .unwrap_or_else(|| panic!("the recorded turn carries a tool call: {transcript:#?}"));
    assert_eq!(tool.status, ToolStatus::Completed, "{tool:#?}");
    assert!(
        tool.input
            .as_ref()
            .and_then(|input| input.get("command"))
            .and_then(|value| value.as_str())
            .is_some_and(|command| command.contains("live-harness-tool")),
        "the recorded command survives: {tool:#?}"
    );
    assert!(
        tool.output.blocks.iter().any(
            |block| matches!(block, crate::backend::ContentBlock::Text(text) if text == "live-harness-tool\n")
        ),
        "the recorded command output decodes into a text block: {tool:#?}"
    );

    // The Session Snapshot tail ends on the final answer, once.
    let tail = transcript.transcript_tail();
    assert_eq!(
        tail.last().map(|entry| entry.text.as_str()),
        Some("live-harness-final-text"),
        "the tail ends on the final answer: {tail:#?}"
    );
    assert_eq!(
        tail.iter()
            .filter(|entry| entry.text == "live-harness-final-text")
            .count(),
        1,
        "the final answer renders once in the tail: {tail:#?}"
    );
}

/// Mount one recorded body on its generation's transcript route and decode it
/// through the real adapter.
async fn decode_recorded(case: &SessionCase, recorded: &RecordedResponse) -> SessionTranscript {
    let server = TestHttpServer::start().await;
    (case.mount_recorded_transcript)(&server, RECORDED_SESSION, &recorded.response.to_string());
    case.backend(&server)
        .transcript(RECORDED_SESSION)
        .await
        .unwrap_or_else(|error| panic!("the recorded body must decode through the adapter: {error}"))
}

/// The completed turn is green on both generations: the same neutral outcome
/// from V1's `{info, parts}` recording and V2's `content[]` one, including the
/// cursor-carrying V2 body (mounted with its terminating page).
#[tokio::test]
async fn recorded_transcript_turns_decode_on_both_generations() {
    for case in [
        crate::opencode::v1::conformance::case(),
        crate::opencode::v2::conformance::case(),
    ] {
        let generation = case.generation.as_str();
        let recorded = match case.generation {
            Generation::V1 => v1_transcript_turn(),
            Generation::V2 => v2_transcript_turn(),
        };
        assert_eq!(recorded.capture.generation, generation, "stamp generation");
        let source = recorded.capture.source.as_str();
        let version = source
            .strip_prefix("opencode ")
            .unwrap_or_else(|| panic!("{generation}: the stamp names the capture source: {source:?}"));
        let version = version.strip_prefix('v').unwrap_or(version);
        assert!(
            version.starts_with(|c: char| c.is_ascii_digit()) && version.contains('.'),
            "{generation}: the source names a version, not just the artifact: {source:?}"
        );
        let live_test = match case.generation {
            Generation::V1 => "live_v1_scripted_capability_chain",
            Generation::V2 => "live_v2_scripted_transcript_read",
        };
        assert!(
            recorded.capture.command.contains("cargo test") && recorded.capture.command.contains(live_test),
            "{generation}: the command names the re-recording live test `{live_test}`: {:?}",
            recorded.capture.command
        );
        assert!(
            chrono::NaiveDate::parse_from_str(&recorded.capture.captured_at, "%Y-%m-%d").is_ok(),
            "{generation}: the capture date is a YYYY-MM-DD date: {:?}",
            recorded.capture.captured_at
        );

        let transcript = decode_recorded(&case, &recorded).await;
        assert_completed_turn(&transcript);
    }
}

/// The V2 mid-turn recording keeps its Turn open: a running tool with no
/// completion stamp, the message still inside the turn (the in-flight rule the
/// streaming card depends on).
#[tokio::test]
async fn recorded_v2_inflight_step_keeps_the_turn_open() {
    let recorded = v2_transcript_inflight();
    assert_eq!(recorded.capture.generation, "v2");

    let case = crate::opencode::v2::conformance::case();
    let transcript = decode_recorded(&case, &recorded).await;

    let user = transcript
        .newest_user()
        .expect("the in-flight recording has a user message");
    let anchor = user.anchor().expect("the user message anchors the turn");
    let turn = transcript.turn_for_user(&anchor);
    assert!(
        !turn.complete,
        "a step that reported no finish has not completed the turn: {turn:#?}"
    );
    assert_eq!(turn.messages.len(), 1, "the in-flight step belongs to the turn");
    assert!(
        turn.messages[0].time.is_some_and(|time| time.completed.is_none()),
        "the in-flight step carries no completion stamp: {:#?}",
        turn.messages[0]
    );
    let tool = turn.messages[0]
        .parts
        .iter()
        .find_map(|part| match part {
            Part::Tool(call) => Some(call),
            _ => None,
        })
        .expect("the in-flight step carries the running tool");
    assert!(tool.status.is_live(), "the tool is still running: {tool:#?}");
    assert_eq!(tool.identity.call_id, "call_live_harness_1", "{tool:#?}");
}
