# Failed card writes retry as Pending Card Updates — newest wins, bounded, in memory

## Context

Card writes are fire-and-forget: a failed `update_message` is logged and
dropped. While a Turn is live the next render poll rebuilds and retries the
same slice, so the gap is exactly the writes around a Turn's end — the final
PATCH, a finalized slice's PATCH before its continuation — and every one-shot
card write outside a Turn (reap endings, interaction repaints, command cards).
Once the poll stops nothing retries: the card stays frozen at its last
delivered state. Observed in a 15-minute Feishu outage (2026-09-16): the
final error card was built and its delivery failed; after the WS reconnected
no reconciliation ran (issue #195). The same failure plus a cola restart
freezes the card forever, because the Live Card record was discarded before
the PATCH (ADR-0063) and nothing else knows the card exists.

## Decision

A **Pending Card Update** (GLOSSARY) is the newest card payload for one card
whose delivery failed and has not since been superseded.

- A delivery decorator wraps `Platform::update_message` where the bridge
  assembles the platform, so every writer is covered: a success clears the
  card's pending entry; a recoverable failure records the payload as the
  card's newest pending update. Entries carry a monotonic per-card sequence,
  so a slow failed retry can never resurrect a superseded payload over a
  newer delivered one.
- Drain: the Session Sync pass retries pending entries with per-entry
  exponential backoff (capped); a Feishu WS reconnect (a default no-op
  `Platform` drain method the ws loop calls) attempts an immediate drain.
  REST and WS reachability are independent, so neither trigger alone
  suffices.
- Bounded: at most one entry per card, newest wins; a small overall cap
  evicts oldest-first with a WARN; entries leave on permanent refusal (card
  gone, content rejected, unrecoverable 4xx) and retry indefinitely
  otherwise — a time limit would strand the card permanently stale, which is
  the defect being fixed. Newest-wins governs the PAYLOAD; the delivered
  evidence is a high-water mark, not newest-only (ADR-0071's amendment,
  review #569): the entry remembers the highest sequence the card delivered,
  so a newer failed write that replaces the entry never unsays an earlier
  delivery a staged Rendered Cursor still confirms by.
- Scope: every `update_message`, not message creation. `reply_card` /
  `send_card` are not retried: Feishu has no idempotency key, so retrying a
  create can double-post. The 230099 content-rejection path keeps its
  fenced→suspend fallback and never enters the pending set; only recoverable
  failures (transport, timeouts, 5xx, 429) do.
- In memory only. OpenCode is the source of truth for content; cola persists
  only the Feishu correlation (the Live Card record). A payload journal was
  considered and rejected; a faithful restart restore is deferred (#505).

The same rule amends ADR-0063's record lifecycle: a terminal card's record
is removed only once its ending write is confirmed (delivered, or permanently
refused), never before the PATCH. A failed final write leaves the record, so
Session Sync's reap finds the card after a restart, re-settles it from
transcript truth (idempotent), and drops the record once the write is
confirmed — the reap doubles as the record's cleaner.

## Considered options

- **Retry inside the flush only.** Rejected: misses every non-flush writer
  (reap stamps, interaction repaints, command cards) and cannot cover a write
  once the Turn's accumulator is gone.
- **Retry creates too.** Rejected: no idempotency, double-post risk.
- **Persist pending payloads (journal).** Rejected: it makes cola a second
  store of content OpenCode already owns, and it still would not resume a
  live run after a restart; the faithful-restore path is the rendered
  watermark, not stored payloads (#505).
- **Give entries a time limit.** Rejected: a card whose newest state never
  landed would stay permanently stale — convergence, not tidiness, is the
  goal; the entry cap is the memory bound.

## Consequences

- A card whose newest write failed converges when Feishu returns (backoff
  during an outage, an immediate attempt on reconnect), including writes made
  as a Turn ended.
- A restart loses pending payloads but not the record: the reap repairs the
  card's state from transcript truth. The body may stay at its last delivered
  content — ADR-0063 already accepts that.
- Permanent failures cannot loop: they are dropped and logged once per
  latched condition.
- Tests pin: a failed final PATCH retried on the next pass; newest-wins over
  a slow failed retry; the reconnect drain; a permanent refusal dropped
  without retry; the cap's eviction; a failed terminal write followed by a
  restart re-settled by the reap.

Related: #195, #443, #505, ADR-0061, ADR-0063, ADR-0065.
