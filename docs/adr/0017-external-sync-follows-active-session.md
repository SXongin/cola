# External-message sync follows the active session of each thread

A p2p (or group) lobby can stack several sessions via `/new`/`/switch`, all mapped
to the same `ThreadKey`. The external-message poller used to notify for every
mapped session, so a message posted from another shared-store client (OpenChamber)
on a historical session popped into the lobby and interleaved its cards with the
current conversation's. We decided external-message sync only follows the ACTIVE
session of each thread.

## Decision

- The external poller (`bridge/external.rs`) notifies and renders only for a
  thread's active session (`SessionStore::get_active`), the same session lobby
  messages route to. Topics hold a single session and are unaffected; p2p and
  group lobbies now sync only the current conversation.
- A session that is not active is not polled, and its baseline is cleared, so
  when the user `/switch`es back to it the poller treats it as first observation
  and re-baselines silently — external messages received while it was inactive
  are marked as read, not replayed.
- `/switch`/`/new` promote a session to active, so the sync target follows the
  user's Feishu session automatically.

## Why

Fixes the interleaving complaint that motivated ADR-0006 in its `/new` form (the
ADR-0008 risk note): stacked lobby sessions no longer mix two conversations'
cards in one chat. The active session is the one the user is "in" on Feishu, so
"sync the active session" is the coherent real-time view.

## Trade-off

Driving a non-active session from another client is no longer surfaced in Feishu
until the user `/switch`es to it. That's the explicit price of no interleaving.

## Deferred

Permission and question cards still deliver to any mapped session (via
`resolve_card_target`), so a blocked external run on a historical session can
still pop a card into the lobby. Scoping those follows the future multi-user
resource-management work (roles, "can I continue someone else's session").
