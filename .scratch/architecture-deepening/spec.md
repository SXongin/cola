# Spec: cola architecture deepening — 7 candidates, 5 waves

Status: ready-for-agent

## Problem Statement

cola works end-to-end, but its internal seams have drifted from its intent. The
Permission and Question flows are ~70% duplicated twins; the Card state concept
is four artefacts kept in lockstep across five files, one of which (the SSE
event fold) is dead code that has already drifted; every flow reaches the whole
bridge coordinator instead of the shared state it actually needs; the
"which server instance" question is a caller-remembered query parameter with a
silent footgun; the Feishu WS per-frame logic is buried in socket-bound code and
untestable, with an unbounded dedupe set; the parent-chain walk is re-implemented
four times; and the four poll loops hard-code their cadence, making their
branches provably untestable.

This is a refactor: behaviour stays identical, the seams move. It was surfaced
by an architecture review (2026-08-29) and sharpened by grilling.

## Solution

Deepen the modules along five independent waves, each ending green (fmt, clippy,
tests, release build) so every wave is independently shippable:

1. **C4 — DirectoryBackend seam**: instance routing becomes a handle.
2. **C3+C1 — SharedCore context + fused RequestFlow**: flows narrow, twins fuse.
3. **C6 — one parent-chain walker**.
4. **C2 — CardSession + dead-code removal**: one persistent Card concept.
5. **C5+C7 — Feishu side**: pure WS frame processor + shared parse helpers.

Two ADRs (0010 Backend seam / 0011 polling-only render) record the
hard-to-reverse decisions. Domain vocabulary is unchanged; CONTEXT.md untouched.

## User Stories

1. As a future contributor, I want the Permission and Question flows to be one
   parameterized flow, so that a request-lifetime bug (double-click, stale card,
   delivery) is fixed once instead of four times.
2. As a future contributor, I want the Card state machine owned by one
   per-session module, so that I can understand "one live card" without reading
   five files.
3. As a future contributor, I want the dead SSE event fold gone, so that I am
   not asked to maintain two competing "turn → card state" implementations.
4. As a future contributor, I want flows to depend on the shared state they use
   (SharedCore) rather than the whole coordinator, so that each flow is
   testable in isolation and map ownership is visible at the seam.
5. As a future contributor, I want the coordinator not to poke another flow's
   private map, so that cross-flow coupling is a method call, not a lock on
   someone else's field.
6. As a future contributor, I want the command↔coordinator module cycle broken,
   so that the dependency graph is acyclic and navigable.
7. As a future contributor, I want the test harness (and its ~4,500 test lines)
   out of the coordinator module, so that the coordinator reads as a
   coordinator, not as an 80%-tests file.
8. As a future contributor, I want instance routing to live on a
   `DirectoryBackend` handle, so that I cannot silently omit `?directory=` and
   scope a request to the wrong server instance.
9. As a future contributor, I want the four parent-chain walks to be one
   walker with a stop predicate, so that the hop-cap policy lives in one place.
10. As a future contributor, I want the WS per-frame processing to be a pure
    function, so that dedupe, age filtering and acks are testable without a live
    socket.
11. As a future contributor, I want the WS dedupe set capped or expired, so that
    the process doesn't grow memory without bound.
12. As a future contributor, I want the live WS path and the quoted-context fetch
    to share their parse helpers, so that "what is this message's text" is
    defined once.
13. As a future contributor, I want poll intervals and the render timeout to be
    fields on the flows, so that every loop branch is testable without sleeping
    real seconds.

## Implementation Decisions

### Wave 1 — C4: DirectoryBackend seam

- `for_directory(dir)` returns a `DirectoryBackend` handle carrying the
  directory; directory-scoped methods (list/reply permissions, list/reply/reject
  questions, session_info) move onto it and take no directory argument. The
  `Backend` trait is kept as the test seam (two adapters: real client + mock);
  `MockBackend` implements both seams. The reconnect loop keeps targeting the
  base URL. ADR-0010 records the trade-off.

### Wave 2 — C3+C1: SharedCore context + fused RequestFlow

- Flows, pollers and the render pipeline take the shared state (`SharedCore`)
  instead of the coordinator; the coordinator keeps owning the flows and the
  event sink.
- The cross-flow map poke (writing the external-flow's user-message baseline
  from the prompt path) becomes a method on the external flow.
- The command↔coordinator module cycle is broken by moving command dispatch to
  depend on `SharedCore` like every other flow.
- The test harness and its ~4,500 test lines move to a `cfg(test)` support
  module, leaving the coordinator readable.
- The Permission and Question flows fuse into one `RequestFlow` parameterized by
  a `RequestKind` trait. The poll loop (sleep → directories → list → seen guard
  → inline-host → flush → else build card → resolve target → reply/send → stale
  sweep), the delivery match, the double-click guard and the error-result block
  live once. Two thin impls keep the deltas: permission's auto-accept and
  description rendering; question's partial-slot accumulation and
  submit/skip/reject. Poll intervals become fields on the flow (defaults = today
  values).
- The merged flow is constructed per kind and spawned once per kind (permissions
  and questions still poll independent endpoints).

### Wave 3 — C6: one parent-chain walker

- A single `walk_parent_chain` helper with a stop predicate replaces the four
  copies (card-target resolution, inline-host resolution, auto-accept walk,
  descendant check). The hop-cap (8) lives in one place.

### Wave 4 — C2: CardSession + dead-code removal

- A per-session, persistent `CardSession` owns the accumulator and the card-id
  chain, replacing the two parallel per-session maps. The prompt path resets
  content per turn but keeps identity; the mid-prompt session-recreate remap
  becomes one operation instead of a three-map re-key.
- The dead SSE fold is deleted: `StreamAccumulator::apply` and the
  `OpenCodeEvent` enum plus its serde fixtures (ADR-0011). The render pipeline
  becomes the single "turn → card state" voice.
- `flush_card` reads/writes the CardSession, so card updates are app-independent.

### Wave 5 — C5+C7: Feishu side

- The WS per-frame processing (dedupe by event id, 5-minute age filter, type
  dispatch, single typed parse, what-to-ack / what-to-dispatch decision) becomes
  a pure `process_frame` function; socket I/O stays in the connection handler.
  The dedupe set gains a cap or time-based expiry.
- The live message path and the quoted-context fetch share one set of
  text/image/mention parse helpers. No unified message model (deferred).

## Testing Decisions

- **The interface is the test surface**: flows are tested across `SharedCore` +
  the two adapter seams — the OpenCode backend(s) (scripted mock, implements
  `Backend` and `DirectoryBackend`) and the Feishu platform (recording mock).
  The existing `build_app` harness already crosses these; the deepening lowers
  the surface so a flow can be constructed with `SharedCore` + mocks alone,
  without the coordinator.
- **No HTTP-level mocking**: the client's URL/scoping is covered by unit tests
  on the request construction where feasible; we do not introduce a mockable
  reqwest layer (ADR-0010).
- **CardSession**: unit tests on state transitions (loading → streaming →
  reasoning → done/error, inline sections), plus `flush_card` against the
  recording platform, reusing the existing continuation-card and dedup tests.
- **process_frame**: pure-function unit tests for dedupe replay, age filter,
  type dispatch, ack/outcome decisions, and dedupe-set eviction — prior art is
  the frame round-trip tests in the WS module.
- **Parent-chain walker**: unit tests over scripted session info, mirroring the
  `is_self_spawned_record` split-pure-core pattern.
- **Poll loops**: driven with tiny injected intervals so every branch (inline
  vs separate card, stale sweep, timeout) is exercised without sleeping real
  seconds.
- Each wave keeps the whole suite green (the ~100 coordinator tests are the
  regression net for the seam moves) before the next wave starts.

## Out of Scope

- Native Feishu card streaming (ADR-0004).
- Real SSE consumption / reviving the v1 event fold (ADR-0011).
- A unified Feishu message model (C7 reduced to shared helpers).
- Abstracting the WS socket for end-to-end frame injection.
- A virtual-time clock abstraction.
- Replacing the Backend trait with a mockable HTTP layer (ADR-0010).
- Any new Feishu permissions or new commands.

## Further Notes

- Two new ADRs (0010, 0011) record the hard-to-reverse decisions; future
  architecture reviews should not re-suggest deleting the trait or reviving the
  SSE fold.
- Domain vocabulary (Permission, Question, Card, Session, Backend, Platform,
  Event) is unchanged — the deepening is pure structure.
- Each wave's definition of done is the standard verification loop: fmt check,
  clippy with -D warnings, full test suite, release build.