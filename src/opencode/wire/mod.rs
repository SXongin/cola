//! Private protocol decoders: one per API generation (ADR-0053).
//!
//! Backend protocol field names live ONLY here. The adapter selects the
//! generation; the Bridge consumes the neutral
//! [`SessionTranscript`](crate::backend::transcript::SessionTranscript) and
//! never branches on a generation. This module ships the legacy (unprefixed
//! routes) decoder; the `/api` (V2) decoder arrives in #339.

pub(crate) mod legacy;

use crate::backend::transcript::SessionTranscript;
use crate::opencode::types::SessionMessage;

/// Decode one session's wire messages through the generation's decoder.
pub(crate) fn decode(messages: &[SessionMessage]) -> SessionTranscript {
    legacy::decode(messages)
}

/// Transitional helper (spec #332): decode a wire-shaped fixture — the JSON a
/// `GET /session/{id}/message` response carries — through the production
/// decoder, so tests can keep scripting wire payloads until their fixtures
/// convert to typed views. Deleted when the fixture migration completes.
#[cfg(test)]
pub(crate) fn transcript_from_wire_fixture(json: &str) -> SessionTranscript {
    let messages: Vec<SessionMessage> = serde_json::from_str(json).expect("wire fixture should parse");
    decode(&messages)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The transitional helper decodes the raw wire JSON through the same
    /// decoder the adapter uses, so a fixture written as wire bytes yields
    /// typed views.
    #[test]
    fn transitional_helper_decodes_wire_json_through_the_production_decoder() {
        let transcript = transcript_from_wire_fixture(
            r#"[
                {"info": {"id": "msg_u1", "role": "user", "time": {"created": 1000}},
                 "parts": [{"type": "text", "text": "你好"}]},
                {"info": {"id": "msg_a1", "role": "assistant", "time": {"created": 1100, "completed": 1200}},
                 "parts": [{"type": "step-finish", "reason": "stop"}]}
            ]"#,
        );

        assert_eq!(transcript.messages.len(), 2);
        assert_eq!(
            transcript.messages[0].role,
            crate::backend::transcript::MessageRole::User
        );
        assert_eq!(transcript.messages[0].text(), "你好");
        let newest = transcript.newest_user().expect("a user message");
        let anchor = newest.anchor().expect("a timed user message anchors");
        assert!(transcript.turn_for_user(&anchor).complete);
    }
}
