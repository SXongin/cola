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
//! The interaction-fact fixtures are the one manual exception: they are
//! sanitized excerpts of unfiltered message reads of real sessions
//! (`?type=idle` is not a legal message filter), so their command records the
//! read and their stamp's `note` records the excerpt and the sanitization.

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
    /// How a recording was derived when the command alone cannot reproduce the
    /// file — the sanitized excerpts taken from a live read, and the
    /// anonymization applied. The live-harness fixtures omit it: their command
    /// re-records them byte-for-byte.
    #[serde(default)]
    pub(crate) note: Option<String>,
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

/// The #403 session's background-wake cycle (2.0.18): a background shell tool
/// part, the shell Wake that retired it, and a later backgrounded run whose
/// Wake had not arrived when the read was taken — one unfiltered transcript
/// read (`?type=idle` is not a legal message filter, so the durable idle
/// boundary rides along), sanitized to neutral ids, times and text (the
/// stamp's `note` records the excerpt and the sanitization).
pub(crate) fn v2_background_wake() -> RecordedResponse {
    RecordedResponse::parse(include_str!("fixtures/v2/background_wake.json"))
}

/// The #403 session's interruption continuation (2.0.18): a `synthetic` that
/// resumed an interrupted response with no metadata at all, the continued
/// steps, and the idle boundary — recorded in the same unfiltered read.
pub(crate) fn v2_interrupt_continuation() -> RecordedResponse {
    RecordedResponse::parse(include_str!("fixtures/v2/interrupt_continuation.json"))
}

/// A real 2.0.18 background-subagent cycle: two `subagent` tool parts that
/// returned their background handles while their runs kept going, the first
/// child's completion Wake (`metadata.source=subagent`, `childID`), and the
/// idle boundaries around them — sanitized like the #403 excerpts.
pub(crate) fn v2_subagent_wake() -> RecordedResponse {
    RecordedResponse::parse(include_str!("fixtures/v2/subagent_wake.json"))
}

#[cfg(test)]
mod tests;
