//! Private protocol decoders: one per API generation (ADR-0053).
//!
//! Backend protocol field names live ONLY here. The adapter selects the
//! generation; the Bridge consumes the neutral
//! [`SessionTranscript`](crate::backend::SessionTranscript) and
//! never branches on a generation. This module ships the legacy (unprefixed
//! routes) decoder; the `/api` (V2) decoder arrives in #339.
//!
//! The wire types are private to their generation's decoder: from outside the
//! adapter only the decode functions are callable, so no protocol field name
//! can escape the seam (spec #332).

pub(crate) mod legacy;

use crate::backend::{Part, SessionTranscript};

/// Decode one session's wire message read — the JSON a
/// `GET /session/{id}/message` response carries — through the generation's
/// decoder.
pub(crate) fn decode_response(json: &serde_json::Value) -> crate::error::Result<SessionTranscript> {
    legacy::decode_response(json)
}

/// Decode one raw parts array — a prompt response's `parts` — through the same
/// generation decoder as a polled message's, so the prompt-response fallback
/// cannot smuggle raw protocol shapes back into the Bridge (spec #332).
pub(crate) fn decode_parts(parts: &serde_json::Value) -> Vec<Part> {
    legacy::decode_parts(parts)
}
