# Turn lifecycle ownership: a deep module behind narrow handles

The second architecture deepening (spec #298) gave the Turn one home. Before
A2, one prompt's lifecycle ran through the coordinator, `turn.rs`, a separate
`render.rs` and the streaming accumulator, and the two halves of a turn met
only inside a mutex map. This ADR records what the Turn owns after the A chain
(#299–#302), so the next architecture review does not re-suggest moving the
render poll back out — or rebuilding the Turn around the aggregate. The
companion narrow-handles decision is ADR-0049 and is not re-litigated here.

## Decision

- **The Turn is a deep module (`src/bridge/turn/`).** Its lifecycle entry is
  `Turn::run(handles, ctx)`. The other operations the coordinator and sibling
  flows drive are cancel, pull-card and follow-external:
  - cancel — `/stop` marks the session in the wait state and the Turn's drain
    observes the marker; no caller reaches into Turn state to stop one;
  - pull-card — `Turn::split_card_chain(…, SplitKind::Pull)` for `/card`;
  - follow-external — `Turn::arm_external_render`, plus the
    `Turn::render_and_flush` the external render loop drives.
- **The render poll, the flush, the Card Chain split, the drain and the
  streaming accumulator are internal to the Turn** (`src/bridge/turn/`). The
  render poll's spawn/stop pairing lives in the private `RenderPoll`
  (`turn/render.rs`); the flush/split state machine is `turn/flush.rs`; the
  accumulator, its card session and their value types are `turn/state.rs`,
  which outside the module is only ever named as the opaque `CardSession`.
- **Outside flows reach card delivery only through `Turn::flush_card` /
  `Turn::split_card_chain` / `Turn::render_and_flush` / `Turn::session_subtitle`.**
  Everything behind those calls — the slice split, the continuation send, the
  card-handle recording, the accumulator edit — is the Turn's implementation.
- **The Turn receives narrow handles (`TurnHandles`), never `SharedCore`**
  (ADR-0049).

## Why

- Understanding "what happens when a user sends a message" is reading one
  module, not four; the phases (`start`, `attempt`, `recreate`, `finish`) and
  their state are private seams.
- The render poll reads the turn's own accumulator and flushes its card through
  the same card-write lock; keeping the spawn/stop pairing next to the state it
  serves is what makes an attempt unable to leak a running poll.
- Sibling flows — a request click's ack, a snapshot re-render, a command — need
  renders and flushes, not turn internals: the four-method surface is where
  their reach stops.

## Considered options

- **The render poll outside the Turn** (a render module owning the task, the
  coordinator spawning and stopping it). Rejected: it re-splits the two halves
  of a turn, and the poll's whole input is Turn state.
- **A coordinator-facing Turn API taking `SharedCore`.** Rejected: it would
  re-widen the Turn's reach, and the narrow handle bundle is what makes its
  dependency set visible at the signature (ADR-0049).
- **Keeping the flush/split and the accumulator as sibling modules.** Rejected:
  they are the Turn's render state machine; a click's ack, a poll and a command
  all reach them through the interface above, and no module outside sees
  accumulator fields.

## Consequences

- A rendering change has one owner; the coordinator and sibling flows call the
  four-method delivery surface and never the internals.
- `Turn::run` is the external test seam; accumulator/render internals have
  private tests inside the module (allowed for a deep module).
- The narrow-handle shape is ADR-0049's decision; the Turn simply never sees
  the aggregate.
