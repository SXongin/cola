# 05 - Adopt-time pending claim (no duplicate; resolving patches the snapshot)

Status: ready-for-agent
Type: task
Blocked by: 02, 03

## What to build

A snapshot that embeds adopt-time pending Permission/Question blocks must not be
duplicated by the request poller. The poller sweeps every SessionStore directory
every 3 s (`src/bridge/request.rs:842-957`); adoption just added the session's
directory, so its next sweep would see the same request as new and surface a
standalone/inline card — unless the snapshot **claims** it first.

## Scope

- **Claim registry in `RequestFlow`** (`src/bridge/request.rs`): a shared
  request_id → owning snapshot-card-id map the poll loop consults BEFORE its
  first-sight dedupe (today that dedupe is the loop's private local `seen`,
  request.rs:843). A claimed id is treated as already-surfaced: no standalone
  card, no re-inline.
- **Only claim what the snapshot actually embedded**: adopt-time requests whose
  `sessionID` == the adopted session AND that are not already surfaced (check the
  flows' `sent_cards` / current inline hosts). Requests that first appear AFTER
  the snapshot was sent are not claimed and keep today's standalone flow
  (ADR-0028).
- **Resolve path**: answering a claimed block via the snapshot's own buttons uses
  the existing request handlers and must remove the block from the snapshot card
  (patch in place) — mirror how inline sections are dropped from a host streaming
  card (`retain_inline`, `drop_surfaced`, `flush_inline_card`, request.rs:…).
  When a claimed request is resolved by ANOTHER client, the snapshot must drop
  the block too — never mark the whole snapshot card "stale" (that patch targets
  standalone cards).
- **Re-switch dedupe**: adopting a session whose pending request was already
  surfaced into this thread before (standalone or inline) embeds nothing new —
  the existing card stays authoritative.
- Whether the static snapshot needs to live behind a lightweight
  `core.cards[session]` host (so retain/flush/patch machinery just works) is an
  implementation decision for this ticket — verify against `StreamAccumulator`
  and `render::flush_card` before choosing; see ADR-0028 Risks.

## Acceptance criteria

- [ ] Adopting a session with a pending permission/question sends ONE card with
      the block embedded; the poller does not pop a duplicate within its next
      sweeps.
- [ ] Answering the block from the snapshot resolves the request and removes the
      block from that card; the card is patched, no new message.
- [ ] Resolving the same request from another client drops the block from the
      snapshot (snapshot itself not marked stale).
- [ ] A request that appears after the snapshot was sent still pops as today's
      standalone card.
- [ ] Re-switch to a session whose pending was already surfaced does not embed a
      duplicate block.
- [ ] `cargo test --workspace --locked` green, clippy/fmt clean.
