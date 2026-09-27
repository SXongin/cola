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
  admit-then-return prompt (explicit `delivery: "steer"`, kept explicit because
  the same submit serves the main dispatch and a mid-turn Supplement, and
  merging into a live run is the desired semantic for both; `steer` is the
  server's own current default). A rejected submit surfaces as the call's
  `Err`.
- **The Turn observes completion.** `attempt` submits and the post-prompt
  drain does the rest: it reads the transcript + run state, renders the turn,
  and treats a settled run's failure as a fact of the transcript (both
  decoders now carry the assistant message's `error` on the neutral read, and
  the newest assistant message's error decides — a recovered earlier step is
  not a failure). The out-of-turn follow reads the same rule: a followed long
  turn that ended in a provider failure finalizes Error, never Done.
- **The drain's settle rule is explicit.** A non-busy status settles the turn
  only once the run was observed to start: a Busy/Retry status, a terminal
  finish, a recorded failure, or a step *created at/after the anchor* (a
  completed straddling step from a prior turn is not a start signal — the
  transcript's membership rule includes a straddler still producing when this
  turn began, so only the created-at/after filter keeps a stale step from
  flipping it). Before that, the retired poll fallback's confirmed-absence
  window applies: after three consecutive non-busy **status** reads the submit
  is treated as never registered and the turn finalizes. At the default 1.5 s
  render cadence that is ~4.5 s, and a reply that arrives after that window is
  deliberately not followed or rendered — the same accepted degradation the
  retired polyfill had. A failing or timing-out **status** read takes the same
  window (so it cannot burn the drain budget plus the follow ceiling). A
  failing **transcript** read is different: before any pending state was
  observed it ends the drain at once (there is no snapshot to act on, so the
  turn finalizes from what it has), and once something was observed it is
  retried to the drain bound. A rejected submit settles on the first non-busy
  read (no run was scheduled). The failure the drain last observed is kept as
  the finalization fallback: if the final reconcile's transcript read fails or
  carries no anchor, that observed failure stands instead of a Done card.
- **The polyfill and its `wait` dependency are deleted.** The experimental
  `POST /api/experimental/session/{id}/wait` call, its confirmed
  `session.active` poll fallback, and the V2 blocking-response assembly are
  removed; so is the `session.wait` note in ADR-0055's Consequences. cola no
  longer depends on the `wait` endpoint (`GET /experimental/session`, ADR-0008,
  is a separate older dependency).
- **V1's submit contract moved, its wire shape did not.** The main dispatch now
  uses the same fire-and-forget `POST /session/{id}/prompt_async` route and
  body the supplement path already used, instead of the blocking
  `POST /session/{id}/message`; the parts/model/variant/agent/messageID payload
  and the 404/error taxonomy are unchanged. The retired blocking response
  assembly (and the V1 wire tests that pinned its inline decode) went with it,
  and the live V1 chains pass against the pinned 1.18.31.

The V2 parity declaration is deliberately NOT made here: that is the gate
ticket (spec #364 S9), which requires the real-Feishu smoke against live V2
plus a V1 regression pass.
