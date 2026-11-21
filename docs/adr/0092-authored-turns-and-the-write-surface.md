# Authorship, steering and the write surface on a client that assigns its own ids

> **The per-client model/provider surface belongs to the module docs and the
> glossary** — `src/codex/model_surface.rs`, `src/backend/types.rs` and
> `GLOSSARY.md` — which record which axis may change where, the catalog, the
> probe and the page. This ADR records the authorship decision and the
> write-surface seam (#1129).

## Context

ADR-0090 made Codex's `app-server` cola's second Agent Client and recorded one
consequence in advance: **authorship needs a new signal.** ADR-0026 tells a
Cola-Authored Message apart by the `msg_cola_…` id cola chooses at send time, and
every OpenCode generation persists that id. Codex assigns every turn and item id
itself, so the prefix never appears, and none of the bridge's authorship answers
can be read off the id.

The write surface (ticket #881) exposed three more places where the bridge had
silently assumed the *message id is cola's*:

- **The Turn's anchor.** `Turn::run` finds a submitted message by the id it
  chose (`anchor_of_user`). When the client renames the message, every Turn
  would look Unreceived even though it landed.
- **The steer/fold boundary.** A Supplement must merge into the live turn.
  Codex spells that `turn/steer` with the active turn's id, while OpenCode's
  steer IS a prompt; the bridge was calling `prompt` with no way to say which.
- **The pick axes.** `/model` and `/think` end up in `prompt`'s `model` and
  `variant`. Codex accepts `model` and `effort` on `turn/start` but has no
  durable session-model switch, unlike V2's `POST /api/session/{id}/model`.

## Decision

**1. The turns cola started are a durable, client-scoped record.** Codex's
adapter writes `authored_turns.json` beside `sessions.json`: the turns it
started, keyed by client and thread, plus the submissions still waiting to bind.
It follows ADR-0091 for scope, version and migration. A turn cola started
answers "cola's" for every user message in it — the protocol carries no
per-message author for a turn — and the durable record survives a restart, the
case the criterion names. A Supplement steered into a turn cola did **not**
start is the exception, recognised by its own item id only (decision 2b): the
turn and its other messages are somebody else's. The file's shape, its caps and
its windows belong to `src/codex/authorship.rs`.

**2. The chosen submission id is durable before the prompt can produce output,
and an ambiguous outcome stays reconcilable.** The turn id only arrives in the
`turn/start` response, but the server can emit the turn's user item before that
response is processed, and the response itself can be lost (the connection
closes after the server accepted the request). `prompt` therefore records the id
it chose for the message (`clientUserMessageId`, a `msg_cola_…` value)
**durably** before the request goes out. Until the turn is bound:

- an item whose `userMessage.clientId` is that chosen id is cola's — a precise
  match no other client's message can satisfy; and
- an item with no client id is cola's only while it belongs to the thread's
  live turn (the pre-response window).

The next history read binds the chosen id to the turn its item landed in, which
is what recovers a lost response across a restart. A **definitely refused**
request — the server answered a JSON-RPC error — clears the marker; every other
failure (closed connection, no response, an undecodable result) keeps it,
because the turn may exist. Pre-send failures are raised before the marker is
written, so a later external turn on the thread is never suppressed by one. A
submission with no chosen id cannot be reconciled at all, so its in-memory
marker is always cleared when the call returns.

**The write is a precondition.** `begin` is fallible: if the chosen id cannot be
made durable (a read-only state directory, an unusable record), the prompt is
refused with a named error and **nothing is sent** — a send the record did not
learn about is exactly the turn a restart would report as external. The valid
existing record stays trusted; only the marker is rolled back. The record
bounds itself with two windows and a per-thread cap, and it refuses the next
prompt rather than evicting an unresolved id; all three are
`src/codex/authorship.rs`'s.

Only an id **this record saw** may vouch. A peer chooses its own
`clientUserMessageId`, so the id's spelling proves nothing: an unfamiliar id —
including one claiming the `msg_cola_` shape — is somebody else's turn, and the
reconciliation never infers authorship from it.

**2b. A Supplement into another client's turn is recorded per item, never per
turn.** `turn/steer` may join a turn the user's TUI started. The adapter records
the steer's chosen id durably as a **steer** submission before the request:

- only the item whose `userMessage.clientId` is that id is cola's; the turn is
  never added to the authored turns, and every other item in it — including an
  id-less TUI message — stays external;
- the thread-wide, id-less fallback never applies to a steer, so it cannot turn
  an unrelated message in the joined turn into cola's;
- a successful response marks the id **resolved**: it stays recorded for
  precise recognition across a restart but no longer counts toward the
  unresolved cap, and the resolved tail is bounded separately;
- a definitely refused steer clears its marker; an ambiguous one keeps it, and
  the next history read resolves it by id;
- `SessionTranscript::submissions` already carries the steer id, so the Turn's
  anchor map and the external-turn boundary both see the message as cola's.

**3. An unusable record fails closed.** A corrupt, unreadable or unwritable
record makes the adapter answer **every** message as cola-authored, with an
`error!` naming the path, and its bytes are left for a human. Silently answering
"external" would notify the user about their own message. An unsupported
declared version is refused at startup, as ADR-0091 requires for every startup
store.

**4. The bridge asks the client, not the id prefix.** `Backend` gains
`is_cola_authored(message_id) -> bool`; its default answers the `msg_cola_`
prefix (every OpenCode client), and Codex overrides it from the record. The four
bridge call sites (Session Sync's notify path, its turn-boundary check, the
snapshot's newest-user classification, the drain's Supplement check) call the
seam. No bridge file decides authorship itself any more.

**5. The client reports which message cola's submission landed as.**
`SessionTranscript` gains `submissions: chosen id → message id`, empty on every
OpenCode read and filled by Codex from `userMessage.clientId` (the field
`clientUserMessageId` populates). `anchor_of_user` resolves through it before
falling back to the message's own id, so the Turn's anchor is still found by the
id cola submitted. This is a *transcript fact*, not a durability mechanism: it
only has to survive the in-process Turn, which is why the authorship record
above does not depend on the server echoing the id.

**6. A write-surface verb is a seam verb, not a prompt.** `Backend` gains
`steer(...)`, whose default delegates to `prompt` (V1's merge, V2's admitted
message keep working unchanged); Codex overrides it with `turn/steer`, and with
`turn/start` when no live turn has been observed. The bridge's Supplement path
calls `steer`. The same rule covers `/skill`, `/stop` and `/compact`: the client
owns the wire (`skills/list` plus the skill's own path, `turn/interrupt`,
`thread/compact/start`), the neutral seam carries the intent, and a capability
the client lacks answers by name instead of silently doing nothing — a skill
that resolves only in the thread's own directory is refused, not substituted
from another project, and a `/stop` racing a just-sent prompt spends a short
bounded lookup so it still interrupts once the server persists the turn.

**7. The card renders a surface the Agent Client declares.**
`Backend::model_surface` is the one source of what `/model` offers and where
each axis may change (`ModelSurface` / `ModelScope`,
`src/backend/types.rs`); the bridge renders it and assumes no shape. That is
what lets a client with no provider tree show one honest scope, and what keeps
the bridge out of any client's identity model. A pick is a next-turn input: it
reaches `turn/start` as `model` and `effort`, and `keeps_session_selection`
stays `false` where the client has no durable switch — `ChangeScope::ThisTurn`'s
meaning. The per-client shape — Codex's global catalog, the neutral `codex/`
bucket, the provider as a configuration fact, `<provider>/<model>` spelling, the
probe and the page — is module behaviour, recorded in
`src/codex/model_surface.rs` and `GLOSSARY.md`.

## Why

The alternative — teaching the bridge about Codex ids — would put a second
client's identity model into every authorship and anchor site, and the seam
audit exists to prevent exactly that. Making the record durable and turn-scoped
is the smallest shape that satisfies the criterion without inventing a
per-message author the protocol does not have. Failing closed is deliberate: the
failure mode this record prevents is user-visible noise about one's own message,
and a broken record must not produce it.

## Consequences

- A steered Supplement is recognised by its own item id, not by claiming the
  turn it joined; the resolved ids are a bounded recognition tail, so a TUI turn
  can never be mistaken for cola's wholesale.
- A cola turn is durable state, not a process-local guess; its file is bounded
  because authorship is only ever asked about the newest user message, and an
  unresolved submission leaves on its own, so the cap cannot wedge a thread
  forever.
- An unresolved chosen id is resolved by the next history read, which is the
  bridge's own read before it asks authorship — so a lost response costs one
  read, not a misclassification. A prompt that cannot record its marker is
  refused, so an unwritable state directory stops prompts (loudly) instead of
  risking a later false External Message.
- A message another client folds into a turn cola started is reported as
  cola-authored too. That is the protocol's granularity, and the alternative is
  guessing an author.
- `Backend` now carries two more defaulted methods (`is_cola_authored`,
  `steer`); an adapter that does not override them keeps its generation's
  behaviour unchanged.
- The transcript carries one more map, empty on every OpenCode read.
- `[bridge]`-owned docs gain the new file in the state table; `/model`,
  `/skill`, `/stop` and `/compact` stay the same Feishu commands, with the
  client owning their wire.
