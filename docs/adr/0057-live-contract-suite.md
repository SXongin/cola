# Live contract suite with a scripted provider (extends ADR-0031)

## Context

ADR-0031's fake-server wire tests pin authored shapes, but V2's protocol is a
rewrite — authored fakes alone cannot be trusted. The retired Feishu E2E needed
an external service, a second bot and network, and rotted. A live check is
needed that is deterministic and self-contained.

## Decision

The live layer runs the real pinned server against an in-process, scripted
OpenAI-compatible provider (SSE, request-recording) in an isolated store and
asserts the resulting capability chain structurally: prompt → streamed
text/reasoning → tool call → permission round-trip → final text. Tests are
`#[ignore]`-gated so the default `cargo test` stays hermetic; local and CI run
the same command. CI jobs (Live V1 / Live V2, or one generation matrix) are
advisory first, pinned by exact version + artifact sha256, and promote into the
required set only once stable. This extends ADR-0031; it is explicitly **not**
a cassette/replay system.

## Consequences

- Live wire reality is exercised in CI without external services or
  credentials; the model is a script, so remaining variance (ids, timestamps)
  is asserted structurally.
- If the V1 spike fails, live checks degrade to documented local runs with
  manual smoke, and CI stays hermetic.

Part of spec #364 (OpenCode V2 dual-generation support).
