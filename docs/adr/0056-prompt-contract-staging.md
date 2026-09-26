# Prompt contract staging: synchronous polyfill first, submit+observe later

## Context

V2 prompts admit durably and return immediately; cola's Turn is built around a
blocking prompt return, though its render/drain already observe the transcript.

## Decision

Keep the trait's synchronous `prompt` for the V2-parity phase (the V2 strategy
polyfills it with `session.wait` plus a poll fallback). After parity, migrate
the Turn to submit+observe as a separate revertible slice; V1 then uses its
native fire-and-forget prompt, V2 its native prompt, and the `wait` dependency
is dropped.

## Consequences

- V2 parity does not coincide with the most delicate Turn refactor.
- The end state is async-native on both generations.
- `session.wait` is the one experimental-endpoint dependency of the migration,
  recorded in ADR-0055, and this slice removes it.

Part of spec #364 (OpenCode V2 dual-generation support).
