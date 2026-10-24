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
{ "version": 1, "turns": { "<client id>": { "<thread id>": ["turn-1"] } } }
```

It follows ADR-0091 for scope, version and migration. Authorship is
**turn-granular** because that is the finest granularity the protocol offers:
a `turn/steer` and a folded `turn/start` both put a user message inside a turn
that already exists, with no per-message author. A turn cola started therefore
answers "cola's" for every user message in it, and the durable record survives
a restart — the case the criterion names.

**2. The record is bound before the prompt can produce output.** The turn id
only arrives in the `turn/start` response, but the server can emit the turn's
user item to the subscription before that response is processed. `prompt`
therefore records a **pending submission** for the thread before the request
goes out; an authorship question for a turn this process has not bound yet is
answered "cola's" while that marker exists. The response binds the turn id and
writes the file before `prompt` returns.

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
own `/compact` request and the item consumes it: the row titles 手动, not 自动,
and a failed request clears the marker so a later automatic compaction is never
mislabelled.

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
  (the newest 128 turns per thread) because authorship is only ever asked about
  the newest user message.
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
