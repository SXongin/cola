# Bridge state access: narrow per-concern handles, with `SharedCore` as the aggregate root

The first architecture deepening (`.scratch/architecture-deepening/spec.md`) made
`SharedCore` the flows' shared context. By 2026-09 the aggregate had grown to 27
fields and every module received it whole: a flow's data reach and its lock
ordering were conventions in caller comments, not something its interface said.
A1 gave the Turn a narrow bundle (`TurnHandles`); this ADR extends the pattern to
every flow and poller and records the shape, so a future architecture review does
not re-open a full `SharedCore` teardown.

## Decision

- **Four per-concern handles, each owning one slice of state and documenting its
  locks.** `SessionsHandle` (the `SessionStore` plus the `/list` cache),
  `CardsHandle` (the live-card map, the ADR-0038 card-handle registry, the cover
  titles, the per-session card-write locks and the platform), `RequestsHandle`
  (the permission and question flows, the double-click guard, the settlement
  claims, the ADR-0028 snapshot-claim registry) and `WaitsHandle` (in-flight
  prompts, the `/stop` marker, the Instant Reminder, the waiting-card pins).
  Handles delegate to the existing stores — the session store and the
  card-handle registry are the prior art — and never duplicate state.
- **Bundles name a flow's dependency set.** `FlowHandles` is the four handles
  plus the backend and the platform (the request and external flows);
  `PollHandles` adds the ADR-0013 server-ownership state (`ServerHandle`);
  `SnapshotHandles` is the read-side subset the snapshot gather and its claim
  filter use; `TopicHandles` and `CommandHandles` add the external flow (and the
  turn config, for the command layer) their adoption tails settle through;
  `TurnHandles` is A1's bundle, unchanged. A bundle is a plain `Clone` struct of
  `Arc`s — it holds no locks of its own.
- **`SharedCore` remains the aggregate root and the single construction point.**
  It owns the `Arc`s, builds every handle and bundle (`sessions_handle`,
  `cards_handle`, `requests_handle`, `waits_handle`, `flow_handles`,
  `poll_handles`, `snapshot_handles`, `topic_handles`, `command_handles`,
  `turn_handles`), and keeps the convenience wrappers the coordinator reads.
  Flows and pollers never receive it; the coordinator (`App`/`handler.rs`) may
  keep `&Arc<SharedCore>` for its own code and hands bundles across module
  boundaries.
- **Lock ownership is part of each handle's interface.** The type-level docs
  state which locks a handle owns and the ordering: the server lock is outermost
  and is never taken under a handle lock; a card write holds the session's
  `write_lock` across the whole read-send-record sequence (`flush_card`,
  `split_card_chain`, `resolve_blocks`); `cards`, `card_handles` and a flow's
  `sent_cards` are taken one at a time and never nested; the claim sets are held
  briefly and never across a backend or Feishu call. The one deliberate
  exception is the reminder and message-pin inner mutexes: each is held across
  its own platform call (`set_instant_reminder`, `pin_message`/`unpin_message`)
  because the decision, the call and the state update are one transition, with
  the failure latch updated inside the guard.
- **Behavior is unchanged.** The work is a mechanical re-plumbing of call sites;
  the test-name set is identical before and after.

## Why

- A flow's reach is now a type: what it can touch and what locks it may take are
  visible at its signature, instead of "everything, by convention".
- Construction stays one call. The wiring lives in `SharedCore`'s accessors, so
  a new flow names a bundle and gets it from the root.
- The existing stores stay the single source of truth; the handles are views, so
  the refactor cannot drift state.
- The upcoming request/command/poll splits (spec #298, tickets C/D/E) start from
  handle-shaped interfaces: a split moves code between modules without
  re-deciding what state each piece may see.

## Considered options

- **A full `SharedCore` teardown** — one root per concern, no aggregate. This is
  the direction future reviews keep suggesting; rejected: construction and
  startup would need many wiring points, and the first deepening deliberately
  made `SharedCore` the flows' shared context. This ADR records the decision so
  the suggestion does not keep returning.
- **One fat "flow context" struct** (a renamed `SharedCore`). Rejected: no
  narrowing — every flow would still see every lock and every field.
- **Passing individual `Arc`s per call site, no bundle types.** Rejected:
  signatures would grow without bound, and a bundle is the place to document a
  flow's dependency set.
- **Encoding lock ownership in the type system** (lock guards or type-state).
  Rejected for now: too invasive for a behavior-identical refactor; types plus
  documented invariants are the proportionate step. Revisit if a lock-order bug
  ever appears.
- **Moving state out of `SharedCore` into the handles** (handles owning the
  `Arc<Mutex<…>>`s rather than cloning them from the root). Rejected: the
  aggregate root must stay the construction point, and two owners of the same
  lock would defeat the "one place" property.

## Consequences

- Adding a concern to a flow means touching `handles.rs` and the root's
  accessors, not just threading the aggregate.
- The coordinator's internal helpers still take `&Arc<SharedCore>`; the boundary
  is "module entry points receive bundles, the coordinator does not".
- Cross-module calls that need a subset construct it cheaply
  (`SnapshotHandles::from_flow`, `CommandHandles::topic_handles`); the `Arc`
  clones are not state copies.
- `CONTEXT.md` is untouched: the domain vocabulary (Turn, Session, Card,
  Request) does not change.
