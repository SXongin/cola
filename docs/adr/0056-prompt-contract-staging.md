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

## Amendment (2026-09-27): the submit+observe end state landed; `session.wait` is gone

The separate, revertible slice the Decision above planned is spec #364's S8. It
landed as follows:

- **The trait's synchronous `prompt` is gone.** One submit method remains: V1
  posts its native fire-and-forget `prompt_async`, V2 its native
  admit-then-return prompt (explicit `delivery: "steer"`; while the session is
  idle it starts the run, while a turn is in flight it merges in at the next
  step boundary). A rejected submit surfaces as the call's `Err`.
- **The Turn observes completion.** `attempt` submits and the post-prompt
  drain does the rest: it reads the transcript + run state, renders the turn,
  and treats a settled run's failure as a fact of the transcript (both
  decoders now carry the assistant message's `error` on the neutral read). The
  drain does not settle on a non-busy status before it has observed the run
  start — a prompt admit only *schedules* execution, so an idle read can race
  the registration; a rejected submit is the exception (no run was scheduled).
- **The polyfill and its dependency are deleted.** The experimental
  `POST /api/experimental/session/{id}/wait` call, its confirmed
  `session.active` poll fallback, and the V2 blocking-response assembly are
  removed; so is the `session.wait` note in ADR-0055's Consequences. Nothing
  in cola depends on an experimental endpoint any more.
- **V1's wire contract is unchanged**: its submits go to the same
  `prompt_async` route and body the supplement path already used. The retired
  blocking `POST /session/{id}/message` response assembly (and the V1 wire
  tests that pinned its inline decode) went with it; the live V1 capability
  chain passes unchanged against the pinned 1.18.31.

The V2 parity declaration is deliberately NOT made here: that is the gate
ticket (spec #364 S9), which requires the real-Feishu smoke against live V2
plus a V1 regression pass.
