# Authorship, steering and picks on a client that assigns its own ids

## Context

ADR-0090 made Codex's `app-server` cola's second Agent Client and recorded one
consequence in advance: **authorship needs a new signal.** ADR-0026 tells a
Cola-Authored Message apart by the `msg_cola_…` id cola chooses at send time,
and every OpenCode generation persists that id. Codex assigns every turn and
item id itself, so the prefix never appears, and none of the bridge's authorship
answers can be read off the id.

The write surface (ticket #881) exposed three more places where the bridge had
silently assumed the *message id is cola's*:

- **The Turn's anchor.** `Turn::run` finds a submitted message by the id it
  chose (`anchor_of_user(&self.cola_message_id)`). When the client renames the
  message, every Turn would look Unreceived even though it landed.
- **The steer/fold boundary.** A Supplement must merge into the live turn.
  Codex spells that `turn/steer` with the active turn's id, while OpenCode's
  steer IS a prompt; the bridge was calling `prompt` with no way to say which.
- **The pick axes.** `/model` and `/think` end up in `prompt`'s `model` and
  `variant`. Codex accepts `model` and `effort` on `turn/start` (and the
  thread's subsequent turns) but has no durable session-model switch, unlike
  V2's `POST /api/session/{id}/model`.

## Decision

**1. The turns cola started are a durable, client-scoped record.** Codex's
adapter writes `authored_turns.json` beside `sessions.json`:

```json
{
  "version": 1,
  "turns":   { "<client id>": { "<thread id>": ["turn-1"] } },
  "pending": { "<client id>": { "<thread id>": ["msg_cola_…"] } }
}
```

It follows ADR-0091 for scope, version and migration: a version-0 body parses and
is rewritten once at load (ADR-0091 §4), and a future version is refused. The
file was never written by a released build, so the only version-0 shape is a
hand-made one in this same form — migrating it is cheaper than teaching a user
to delete it, and refusing it would make a harmless file block startup.
Authorship is
**turn-granular** because that is the finest granularity the protocol offers:
a `turn/steer` and a folded `turn/start` both put a user message inside a turn
that already exists, with no per-message author. A turn cola started therefore
answers "cola's" for every user message in it, and the durable record survives
a restart — the case the criterion names.

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
because the turn may exist. Pre-send failures (an unknown skill) are raised
before the marker is written, so a later external turn on the thread is never
suppressed by one. A submission with no chosen id cannot be reconciled at all,
so its in-memory marker is always cleared when the call returns.

**3. An unusable record fails closed.** A corrupt, unreadable or unwritable
record makes the adapter answer **every** message as cola-authored, with an
`error!` naming the path, and its bytes are left for a human. Silently answering
"external" would notify the user about their own message. An unsupported
declared version is refused at startup, as ADR-0091 §5 requires for every
startup store.

**4. The bridge asks the client, not the id prefix.** `Backend` gains
`is_cola_authored(message_id) -> bool`; its default answers the `msg_cola_`
prefix (every OpenCode client), and Codex overrides it from the record. The
four bridge call sites (Session Sync's notify path, its turn-boundary check,
the snapshot's newest-user classification, the drain's Supplement check) call
the seam. No bridge file decides authorship itself any more.

**5. The client reports which message cola's submission landed as.**
`SessionTranscript` gains `submissions: chosen id → message id`, empty on every
OpenCode read and filled by Codex from `userMessage.clientId` (the field
`clientUserMessageId` populates). `anchor_of_user` resolves through it before
falling back to the message's own id, so the Turn's anchor is still found by
the id cola submitted. This is a *transcript fact*, not a durability
mechanism: it only has to survive the in-process Turn, which is why the
authorship record above does not depend on the server echoing the id.

**6. A Supplement is a seam verb, not a prompt.** `Backend` gains
`steer(...)`, whose default delegates to `prompt` (V1's merge, V2's admitted
message keep working unchanged); Codex overrides it with `turn/steer`,
`expectedTurnId` taken from the cached live turn, and `turn/start` when no live
turn has been observed (the server folds that into the active turn anyway).
The bridge's Supplement path calls `steer`.

**7. Model and effort ride the turn.** `/model` lists `model/list` under one
neutral provider bucket (`codex` — the protocol names no provider, and a false
"openai" would be a claim the adapter cannot make); each model's
`supportedReasoningEfforts` become its `variants`, so `/think` offers exactly
the server's effort set. A pick reaches the next `turn/start` as `model` and
`effort`. `keeps_session_selection` stays `false` and `switch_session_model`
stays the trait's no-op: Codex has no durable switch, so the per-turn shape is
the honest one — V1's shape, not V2's.

**8. `/skill` resolves through the list and the protocol's path.** `skills/list`
is the source for `/skill`; Codex's `skill` input names a **path** the neutral
`PromptSkill` does not carry, so the adapter keeps the last list's
`name → path` catalog and fails a prompt that names a skill the last read did
not. Codex has no separate skill id, so the name is the neutral id (V1's rule).

**9. `/stop` and `/compact` use the protocol's own verbs.** `turn/interrupt`
names the live turn; when the cache has not observed it — the subscription
began after `turn/started`, or the start is still in flight — the adapter reads
the thread's persisted turns and interrupts the `inProgress` one, so a stop
right after a prompt is not a silent no-op. `thread/compact/start` is the
compact; the resulting `contextCompaction` item projects as the neutral
Compaction the existing card path renders exactly once (ADR-0082's Rendered
Cursor rule). The protocol's item carries no reason, so the adapter records its
own `/compact` request and the item consumes it: the row titles 手动, not 自动.
Only a definite refusal clears the marker; an ambiguous failure keeps it,
because the compaction may have started and its item still needs the right
title.

## Considered options

- **Read authorship from the server's `userMessage.clientId` alone.** Rejected
  as the whole answer. It is the precise per-message signal and the adapter does
  use it, but it lives only in the server's item stream and rollout: a restart
  may read a history page without it, and a client that never sends the field
  gives no signal at all. The durable turn record is independent of it.
- **Keep the chosen ids in memory only (the first cut).** Rejected on review: a
  restart between `turn/start` and the response would lose the only link to the
  turn, and cola's own message would notify. Recording the id durably changes a
  lost response from a silent misclassification into a reconcilable state.
- **Abandon the pending marker on every error.** Rejected: a closed connection
  after acceptance is indistinguishable from a refusal at the client, so
  abandoning it on transport failures re-opens the same hole. Only a JSON-RPC
  error is positive evidence the turn did not start.
- **Scope the skill catalog per prompt directory.** Rejected because `prompt`
  carries no directory on the neutral seam. The catalog is scoped per directory
  by its reads and resolves a name to the most recently read scope, which is the
  one the `/skill` command just refreshed; a skill deleted from every scope is
  refused.
- **Make `/model` durable with a thread-level setting.** Rejected: Codex's
  schema accepts `model`/`effort` on `turn/start` and has no session-model
  switch to mirror V2's. Inventing a client-side durable setting would make the
  footer and the server disagree the first time another client changes it.
- **A per-message authorship table keyed by item id.** Rejected: item ids are
  only known after the item arrives, so the table could not be written before
  the request — the exact window it would have to cover.

## Why

The alternative — teaching the bridge about Codex ids — would put a second
client's identity model into every authorship and anchor site, and the seam
audit exists to prevent exactly that. Making the record durable and turn-scoped
is the smallest shape that satisfies the criterion without inventing a
per-message author the protocol does not have. Failing closed is deliberate:
the failure mode this record prevents is user-visible noise about one's own
message, and a broken record must not produce it.

## Consequences

- A cola turn is durable state, not a process-local guess; its file is bounded
  (the newest 128 turns, and 32 unresolved submissions, per thread) because
  authorship is only ever asked about the newest user message.
- An unresolved chosen id is resolved by the next history read, which is the
  bridge's own read before it asks authorship — so a lost response costs one
  read, not a misclassification.
- A message another client folds into a turn cola started is reported as
  cola-authored too. That is the protocol's granularity, and the alternative is
  guessing an author.
- `Backend` now carries two more defaulted methods (`is_cola_authored`,
  `steer`); an adapter that does not override them keeps its generation's
  behaviour unchanged.
- The transcript carries one more map, empty on every OpenCode read.
- A `/stop` on an idle thread spends a short bounded lookup (about half a
  second at worst) before it answers, because a stop racing a just-sent prompt
  must still interrupt the run once the server persists it.
- `[bridge]`-owned docs gain the new file in the state table; `/model`,
  `/skill`, `/stop` and `/compact` stay the same Feishu commands, with the
  client owning their wire.
