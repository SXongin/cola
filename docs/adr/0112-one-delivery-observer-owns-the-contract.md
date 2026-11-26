# One delivery observer owns the contract

> **Amended by ADR-0115**: `DeliveryObserver` declares a twelfth required
> method, `owe_rate_limited_write`, which claims a terminal body Feishu's
> frequency limit proved unsent as the card's keyless Pending Card Update. The
> one-owner rule and the no-op's answers below are unchanged.

> **Amends ADR-0072**: one clause — `Platform::submit_ordered`'s default
> `update_message` fallback for an unwrapped platform — is superseded. The
> decorator's ownership of per-card write order stays as ADR-0072 recorded it.

## Context

ADR-0072 (spec #571) moved per-card write order into the delivery decorator and
gave the `Platform` trait a convenience beside it: `submit_ordered` defaulted to
a straight `update_message`, so an unwrapped platform — a test fake, a
hypothetical adapter — still compiled and wrote something.

That default was a **shadow contract**. It wrote the payload unordered: no
per-card serialization, no stale-generation drop, no settled
`(generation, intent)` collapse, no owed/retry schedule, no fallback under the
same card lock. A platform that never opted in therefore delivered differently,
silently — the failure the ordering exists to prevent.

In production the fallback was unreachable: one assembly point wraps the raw
transport (`src/bridge/core.rs:329`). But it was reachable from tests and fakes,
where "wrote something" was indistinguishable from "wrote it in order", and it
kept the queue's guarantees in a default nobody was forced to take. The
refactoring charter (#1194, C7; ADR-0110) named it as one owner too many — the
delivery contract had two homes, the decorator and the trait default.

## Decision

**`DeliveryObserver` (the decorator's contract) is the one owner of delivery.**
It declares the eleven methods as **required**, with no default bodies:
`submit_ordered`, `keyed_write_covered`, `keyed_ticket_await`,
`keyed_write_settled`, `drain_pending_card_updates`, `has_pending_card_update`,
`pending_card_write`, `card_write_delivered`, `failed_card_write`,
`settled_card_write_delivered` and `defer_notice_until_delivered`.
`CardDelivery` implements it; the previous `Platform` defaults are deleted.

`Platform` keeps exactly **one** delivery default:

```rust
fn delivery(&self) -> &dyn DeliveryObserver { &UNOBSERVED }
```

`UNOBSERVED` is the one documented no-op, and it is conservative and visible:

- an ordered submission logs a WARN naming the card and returns
  `WriteOutcome::Failed(BridgeError::Feishu("ordered delivery is unsupported by
  this Platform"))` (`src/feishu/delivery/observer.rs:271`) — a permanent
  refusal (`src/error.rs:17-18`), never `Delivered` (a false claim the write
  landed) and never `Superseded` (which reads "a newer write owns it");
- every read reports "nothing owed / never delivered" (`false` / `None`);
- the drain has no queue to walk, so it writes nothing;
- `keyed_ticket_await` keeps the production bound `KEYED_TICKET_AWAIT` (a
  caller bound, not a state claim);
- `defer_notice_until_delivered` keeps `NoticeGate::Delivered`, the
  pre-existing no-queue answer. It is unreachable on the no-op in practice: the
  gate is only consulted with a sequence `pending_card_write` /
  `failed_card_write` reported, and both answer `None`.

**Boundary.** The keyless `update_message` class stays on `Platform`: the
decorator's override keeps owning it, because an observer with no transport
cannot write a card. `delivery()` is the only seam; the decorated path's
semantics — stale-generation drop, the settled collapse, owed → failed →
retried, newest-wins, caps, backoff, the drain — are unchanged.

## Consequences

- **An unwrapped platform now fails an ordered write instead of writing it
  unordered.** That is the point: the difference is loud instead of silent.
  Tests and fakes either run through the decorator or take the documented no-op
  on purpose.
- One owner: every behaviour a removed `Platform` default carried is reachable
  through `DeliveryObserver` and implemented once, by `CardDelivery`; the no-op
  exists once, as `UNOBSERVED`.
- The failure is observable and pinned by a bridge fixture over a raw platform
  (`a_platform_without_an_observer_fails_the_ordered_write_visibly`,
  `a_platform_without_an_observer_reports_no_delivery_state`).
- In-process only: no wire or durable-format change (ADR-0072's scope stands).

Related: #1139, spec #1194 (charter C7), ADR-0072, ADR-0067, ADR-0063, ADR-0071,
ADR-0110.
