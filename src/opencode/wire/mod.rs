//! The recorded-response fixture corpus (spec #364, "Testing Decisions").
//!
//! Named for the wire responses it stores — it contains no protocol logic; the
//! generation decoders live in each strategy's own `wire` module. A small
//! sanitized corpus of upstream-owned read shapes, recorded from the live
//! harness's real servers and committed under `fixtures/<generation>/`.
//! Each file is stamped with the generation, the server artifact that produced
//! it (`source`), the exact command to re-record it and the capture date; the
//! stamp travels in-band so a decoder test can assert it.
//!
//! There is deliberately **no replay engine**: a test mounts the recorded
//! `response` on the generation's route and lets the production decoder read
//! it (ADR-0031's cassette rejection stands). The loader itself is test-only —
//! the corpus never ships in a release build.
//!
//! Re-record with the live harness's `COLA_LIVE_CAPTURE_DIR` hook (see
//! `crate::opencode::live`), sanitize ids/times/cursors by hand, and commit.

use serde::Deserialize;
use serde_json::Value;

/// One committed recording: the sanitized response plus its capture stamp.
#[derive(Debug, Deserialize)]
pub(crate) struct RecordedResponse {
    pub(crate) capture: CaptureStamp,
    pub(crate) response: Value,
}

/// Where and how a recording was made.
#[derive(Debug, Deserialize)]
pub(crate) struct CaptureStamp {
    /// The generation the server spoke (`v1` / `v2`).
    pub(crate) generation: String,
    /// The server artifact that produced the recording (`opencode 1.18.31`).
    pub(crate) source: String,
    /// The live-harness command that re-records this file.
    pub(crate) command: String,
    /// The UTC date of the capture.
    pub(crate) captured_at: String,
}

impl RecordedResponse {
    /// Parse one committed fixture. A malformed fixture is a loud test failure,
    /// never a skipped corpus.
    fn parse(raw: &str) -> Self {
        serde_json::from_str(raw).expect("a committed fixture must be valid JSON with a capture stamp")
    }
}

/// The V1 live capability chain's completed turn (pinned 1.18.31): a user
/// message, the tool-calling step and the closing text step, in V1's
/// `{info, parts}` envelope.
pub(crate) fn v1_transcript_turn() -> RecordedResponse {
    RecordedResponse::parse(include_str!("fixtures/v1/transcript_turn.json"))
}

/// The V2 live read chain's completed turn (2.0.18): a user message, the
/// tool-calling step, the closing text step and the idle marker, in V2's
/// `content[]` envelope.
pub(crate) fn v2_transcript_turn() -> RecordedResponse {
    RecordedResponse::parse(include_str!("fixtures/v2/transcript_turn.json"))
}

/// The V2 live read chain's mid-turn step: a running tool with no completion
/// stamp and no `finish` — the in-flight shape turn membership turns on.
pub(crate) fn v2_transcript_inflight() -> RecordedResponse {
    RecordedResponse::parse(include_str!("fixtures/v2/transcript_inflight.json"))
}

#[cfg(test)]
mod tests;
