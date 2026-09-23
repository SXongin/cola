# Logs: session-scoped tracing spans, with payload dumps demoted and retries warning once

Finding a Session's logs meant correlating timestamps by eye: session ids
appeared only in some hand-written messages, the highest-volume paths (render
polls, turn finalization, WS events, card actions, message pins) carried none,
and no code ever entered a `tracing` span. INFO also carried every payload
dump. This ADR makes the Session the unit of log correlation — session-scoped
work enters a span whose `session` / `chat` / `topic` fields the default fmt
layer prefixes to every line inside it — and records the level policy that
keeps those lines worth reading.

## Decision

- **Spans, not per-event fields.** Session-scoped code enters a `tracing` span
  (`.instrument()` for spawned tasks); the fmt layer's default span prefix
  (`turn{session=ses_x chat=oc_x}: …`) is the rendering. No custom
  `FormatEvent`, no JSON log layer.
- **Three short fields.** `session` (`ses_…`), `chat` (`oc_…`), `topic`
  (`omt_…`; omitted when the message is not in a topic). Directory and other
  long data never ride the per-line prefix — they belong to the turn-start
  anchor line.
- **Every flow that knows a Session carries it.** `Turn::run` covers the
  prompt, its awaited Backend calls, the drain and the finalization; the render
  poll (a separate task) is instrumented explicitly; the permission and
  question pollers create a span per request they surface; card actions enter
  one once their session is resolved; external-message sync/render and the
  snapshot/topic flows instrument per session. The inbound WS message path
  carries `chat`/`topic` only — no Session exists at receipt.
- **One INFO turn-start anchor per Turn.** session/chat/topic/directory plus
  the prompt's character count, never its body. The existing inbound-message
  preview (`Message: … text=<first 50 chars>`) stays at INFO: it is the only
  line that says which message triggered the turn.
- **Level policy.** Full payload dumps (the WS event payload, the prompt
  response body), ack-timing lines and exact duplicates are DEBUG. State
  transitions stay INFO: turn start/end, request surfaced or auto-accepted,
  card split, session retitled. The render poll keeps INFO — it only fires when
  new content arrived, and is the per-Session liveness heartbeat a trace is
  read for.
- **Retry loops warn once, not every tick.** A best-effort call that keeps
  failing logs WARN on the first failure with the actionable cause (for a
  missing Feishu scope, the scope's name), the same failure at DEBUG after
  that, and INFO on recovery. Retry cadence is unchanged — some failures are
  transient (pinning is not idempotent), so self-healing stays. Applied first
  to Instant Reminder and Message Pin, whose 3-second sweeps would otherwise
  WARN for as long as a request waits.
- **Retrieval is grep.** `rg 'session=ses_x' ~/.cola/cola*.log`. No `cola logs`
  CLI, no in-chat `/log` command.
- **The convention is tested.** A test-side capture layer (a custom
  `MakeWriter` over the existing `tracing-subscriber` dependency — no new
  dependency) asserts representative paths emit `session=`/`chat=`.

## Considered options

- **Explicit `session=` fields on every log call.** No spans; ~30–50 call sites
  to touch, and logs emitted in the Backend client layer would need context
  threaded through function arguments. Spans inherit for free within a task,
  including awaited calls — rejected.
- **A custom `FormatEvent` fed by a context registry.** Global state and a
  nonstandard formatter for no gain over the default span prefix — rejected.
- **A JSON log layer for structured queries.** The one consumer reads by eye or
  with `rg`; text plus a span prefix stays scannable. Additive later if a
  machine consumer appears — rejected for now.
- **Per-Session log files.** File-handle and rotation complexity, plus
  cross-cutting lines (messages before a Session exists) that belong to no
  file — rejected.
- **Demoting the render poll to DEBUG with a coarse periodic progress line.**
  Loses exactly the per-tick liveness signal a session trace is read for, and
  the line only fires on progress — rejected.
- **Latching Instant Reminder / Message Pin off on a permission-class error.**
  Classifying Feishu error codes by guess is brittle, and a granted scope would
  need a restart. Warning-once removes the spam with no behavior change —
  rejected for now; revisit if the futile API-call volume ever matters.
- **`cola logs <session>` / `/log`.** grep already answers the need; a helper
  is a follow-up issue if it becomes high-frequency — rejected for now.

## Consequences

- `EnvFilter` cannot select by span field value: filtering by session is grep,
  not `RUST_LOG`. Per-target level tuning (`RUST_LOG=cola::bridge::render=debug`)
  still works.
- Every line inside an instrumented task gains a `span{fields}:` prefix; log
  volume is otherwise unchanged except where lines were demoted.
- The capture test pins representative paths only; new log sites elsewhere rely
  on review to keep the convention.
- CONTEXT.md is untouched: it is a glossary, and log fields are implementation
  detail.
