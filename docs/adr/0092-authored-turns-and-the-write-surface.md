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
A turn cola
started answers "cola's" for every user message in it — the protocol carries no
per-message author for a turn — and the durable record survives a restart, the
case the criterion names. A Supplement steered into a turn cola did **not**
start is the exception, recognised by its own item id only (decision 2b): the
turn and its other messages are somebody else's.

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

**2b. A Supplement into another client's turn is recorded per item, never per
turn.** `turn/steer` may join a turn the user's TUI started. The adapter records
the steer's chosen id durably as a **steer** submission before the request:

- only the item whose `userMessage.clientId` is that id is cola's; the turn is
  never added to the authored turns, and every other item in it — including an
  id-less TUI message — stays external;
- the thread-wide, id-less fallback never applies to a steer, so it cannot turn
  an unrelated message in the joined turn into cola's;
- a successful response marks the id **resolved**: it stays recorded for
  precise recognition (across a restart) but no longer counts toward the
  unresolved cap; the resolved-steer tail is bounded separately (256 per
  thread, oldest dropped), while an unresolved steer obeys the same one-hour
  valve and lost-response reconciliation as a prompt;
- a definitely refused steer clears its marker; an ambiguous one keeps it, and
  the next history read resolves it by id;
- `SessionTranscript::submissions` already carries the steer id (the server
  echoes `clientUserMessageId` as `userMessage.clientId`), so the Turn's anchor
  map and the external-turn boundary both see the message as cola's.

**The write is a precondition.** `begin` is fallible: if the chosen id cannot be
made durable (a read-only state directory, an unusable record), the prompt is
refused with a named error and **nothing is sent** — a send the record did not
learn about is exactly the turn a restart would report as external. The valid
existing record stays trusted; only the marker is rolled back. At the per-thread
cap (32 unresolved ids per thread) the next prompt is refused too: silently
evicting an unresolved id is the same loss.

Two windows bound the record. The **id-less, thread-wide fallback** stops
vouching after five minutes (shorter in tests): a request that never reached the
server must not make a later foreign turn with no client id look cola's. The
**precise recorded id** outlives that fallback — a lost response that surfaces
later than five minutes still binds — but it too has a recorded life: an entry
older than **one hour** (shorter in tests) no longer vouches, and the history
read **prunes before it reconciles**, so an aged id cannot bind an item even on
the first read after the thread sat idle. The one-hour valve is also what bounds
the pending set when resolution never arrives; until it fires, the per-thread
cap refuses new prompts rather than forgetting an outstanding id.

The trade-off is deliberate. Within the hour, a recorded id is authoritative.
Past it, the entry is pruned and a genuinely lost response that surfaced after an
hour would be read as an External Message — one unnecessary notification about
cola's own message. The alternative is worse: keeping an aged, peer-reusable id
would let a peer suppress its own turn indefinitely by reusing it. The server
persists an accepted turn promptly, so an hour is far longer than any real
response needs.

Only an id **this record saw** may vouch. A peer chooses its own
`clientUserMessageId`, so the id's spelling proves nothing: an unfamiliar id —
including one claiming the `msg_cola_` shape — is somebody else's turn, and the
reconciliation never infers authorship from it.

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
`PromptSkill` does not carry, so the adapter keeps one `name → path` scope per
directory it has read. Each read replaces its own scope, so a deleted skill
stops resolving. A prompt resolves **only** in the thread's own `cwd` (learned
at creation, adoption, or a metadata read taken before a skill prompt when it is
still unknown); without that directory, every skill is refused by name — a
skill advertised for another project is never substituted, because its path and
even its content may differ. Codex has no separate skill id, so the name is the
neutral id (V1's rule).

**8b. The skill prompt's metadata read is bounded.** When the adapter has not
learned the thread's `cwd` before a skill prompt, the `thread/read` that
supplies it runs under its own timeout (two seconds; shorter in tests). A
half-open connection must not hold the prompt before submission; an expired or
failed read is a named error, and no skill prompt is sent without the
directory.

**9. `/stop` and `/compact` use the protocol's own verbs.** `turn/interrupt`
names the live turn; when the cache has not observed it — the subscription
began after `turn/started`, or the start is still in flight — the adapter reads
the thread's persisted turns and interrupts the `inProgress` one, so a stop
right after a prompt is not a silent no-op. The lookup is polled within a
bounded window and every read has its own deadline, so a stop either lands when
the turn appears or answers as idle within about half a second — a stalled RPC
cannot hold it. `thread/compact/start` is the
compact; the resulting `contextCompaction` item projects as the neutral
Compaction the existing card path renders exactly once (ADR-0082's Rendered
Cursor rule). The protocol's item carries no reason, so the adapter records its
own `/compact` request and the item consumes it: the row titles 手动, not 自动.
Only a definite refusal clears the marker; an ambiguous failure keeps it,
because the compaction may have started and its item still needs the right
title. The marker is process-local on purpose: it is a display hint, so a
restart before the item projects titles that one row 自动 rather than adding a
durable shape for a label.

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
- **Record nothing for a steer (the first cut).** Rejected on review: the
  supplement's item then answers "external", so Session Sync announces cola's
  own message and the external renderer opens a new boundary inside the turn.
- **Record a steer as an ordinary prompt.** Rejected: a prompt marker turns on
  the thread-wide fallback and binds the whole turn, which would make the TUI's
  own id-less messages in that turn cola's. The steer kind is precise by
  construction.
- **Bind any `clientId` that looks like cola's `msg_cola_` id.** Rejected on
  review: a peer chooses its own `clientUserMessageId`, so the prefix is a claim,
  not evidence. Only a recorded id may vouch; the lost-response case is kept by
  recording the id before the send, not by trusting its spelling.
- **Fall back to another directory's scope for a skill.** Rejected: a
  same-named skill in another project is a different file with a different
  body, and sending it silently would run the wrong instructions. The adapter
  reads the thread's `cwd` before a skill prompt instead, and refuses when it
  cannot establish the directory.
- **Expire the precise verdict with the id-less fallback.** Rejected: a lost
  response can surface later than five minutes, and the precise id cannot
  collide with anything a peer sent unless that peer deliberately reuses the
  recorded id (which is cola's claim, not the peer's). A separate one-hour
  safety valve bounds the set, and the precise answer is age-checked so the
  valve protects the very first read after an idle period; the cap refuses new
  prompts before then.
- **Evict the oldest unresolved submission at the cap.** Rejected: an
  unresolved id may still be owed, and dropping it is exactly the provenance
  loss the record exists to prevent. The next prompt is refused instead.
- **Persist the manual-compaction hint.** Rejected: it is a label (手动 vs
  自动) on a row whose fact — the compaction item — is already durable and
  rendered either way. A second durable shape for a display hint adds a failure
  mode without changing what the card shows.
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

- A steered Supplement is recognised by its own item id, not by claiming the
  turn it joined; the resolved ids are a bounded recognition tail, so a TUI turn
  can never be mistaken for cola's wholesale.
- A cola turn is durable state, not a process-local guess; its file is bounded
  (the newest 128 turns, and 32 unresolved submissions, per thread) because
  authorship is only ever asked about the newest user message. A submission
  that never resolves leaves after an hour, so the cap cannot wedge a thread
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
- A `/stop` on an idle thread spends a short bounded lookup (about half a
  second at worst) before it answers, because a stop racing a just-sent prompt
  must still interrupt the run once the server persists it. Each read has its
  own deadline, so a stalled RPC cannot extend the wait.
- `[bridge]`-owned docs gain the new file in the state table; `/model`,
  `/skill`, `/stop` and `/compact` stay the same Feishu commands, with the
  client owning their wire.

## Amendment (2026-10-27, #904): the running model is a thread fact

Decision 7 left the footer and the picker's current-model line with nothing to
show on Codex, because all three neutral rungs were empty:
`configured_default_model` returns `None`, `session_info` hard-coded
`model: None`, and every transcript message carried `model: None`. The protocol
does carry the fact — at the **thread** level: `thread/list` and `thread/read`
report `model` + `modelProvider` (+ `reasoningEffort`), and `thread/start`
accepts them. `TurnStartParams.model` is *"for this turn and subsequent turns"*,
so the thread's model is both what the last turn ran and what the next one will
run: one source for both surfaces.

The adapter now reads it: `Thread` decodes `model`/`modelProvider`;
`ThreadProjection` stamps it onto **assistant** messages (the footer's existing
`capture_footer_model` path, so no bridge change); `session_info` reports it (the
`effective_selection` ladder's third rung, so `/model`'s current-model line
works). Three consequences of that shape:

- The picker's **button values** stay the neutral `codex/<model>`, because that
  is the reference cola sends back; only the *display* uses the real provider —
  which is what `/model`'s current-model line and the footer show. An absent
  `modelProvider` falls back to the same neutral bucket, never to "openai".
- The projection's model moves only where the protocol says it moves: a thread
  read / start / resume, and cola's own `turn/start` carrying a pick (its
  response is the acceptance — a refused turn changed nothing). A peer client
  switching the model mid-thread stays unseen by the footer until the next
  thread read; the picker reads the thread each time, so it is always current.
  `model/rerouted` is the notification that would close that gap if it ever
  matters; not adopted here.
- With no thread model on the wire, nothing is shown. Filling it from
  `~/.codex/config.toml` would be a different read (`config/read`) and is left
  out: a fact cola does not have must not be presented as the server's.

Also out of scope: offering `[model_providers.*]` models in the picker.
`model/list` is a flat, provider-less catalog compiled into the CLI (it names no
provider and does not include custom providers' models), so cola cannot offer
choices its own pick path could not send.
