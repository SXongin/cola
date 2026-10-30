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
- **What silence means depends on where the fact came from**, and the
  projection stores the source for exactly that reason. An absent (or null —
  indistinguishable on the wire) `model` is not evidence that the model
  changed: it drops a name the server itself gave earlier, because an old name
  would claim the wrong model answered, but it never erases a model cola sent
  on an accepted `turn/start` — a server that omits the field must not undo
  the user's own pick. A reported model always wins over either, so a peer's
  switch lands on the next read.
- **And a fact has an age**, because reads and picks can overlap: the stamp is
  taken *before* the read that will learn the fact (or at the pick's
  acceptance), and a fact older than the one the projection holds is dropped.
  Without it, a `/model` read that started before the pick and answered after
  it would put the previous name back on the footer — the same stale-name
  failure one interleaving later. The projection takes a fact only when it is
  at least as new as what it holds; a read that starts after a pick still wins,
  which is exactly how a peer's switch lands. Both surfaces read the ordered
  store, not their own response, so the `/model` line cannot disagree with the
  footer. The trade is deliberate: a read whose answer reflects state newer
  than the pick is dropped too, and the next read repairs it — showing the
  pre-pick model is the failure that does not self-heal in front of the user.
- **A pick keeps the thread's real provider.** A picker value carries the
  neutral `codex` bucket (it is the reference cola sends back), but a model
  override does not change `model_provider` — that is configuration, not part
  of `turn/start`. So a provider the thread already named wins the label, and
  only a thread that never named one shows the neutral bucket.
- *(Superseded by spec #922, 2026-10-30.)* A projection released with its
  thread no longer re-sends a pick on its own: the one-shot pending is the only
  state that carries a pick, and it is consumed by the next new turn's
  acceptance. The projection is a display fact, not a dispatch input.
- With no thread model ever named, nothing is shown. Filling it from
  `~/.codex/config.toml` would be a different read (`config/read`) and is left
  out: a fact cola does not have must not be presented as the server's.

Also out of scope: offering `[model_providers.*]` models in the picker.
`model/list` is a flat, provider-less catalog compiled into the CLI (it names no
provider and does not include custom providers' models), so cola cannot offer
choices its own pick path could not send.

## Amendment (2026-10-29, #906): the model surface is the client's to declare

Decision 7 and the first amendment both stopped at the same edge: `/model`
showed `model/list` under one neutral `codex` bucket, and `[model_providers.*]`
stayed out of scope because the catalog "cannot offer choices its own pick path
could not send". Spec #906 measured what was actually on the other side and
found the **shape** was the defect: the card rendered OpenCode's
provider → model tree over a client that has no such tree, which is where
「选择 provider（共 1 个）→ codex」 came from. The surface now belongs to the
**Agent Client**, the card only renders it, and the first amendment's closing
out-of-scope line is superseded.

**1. `Backend::model_surface` is the one source, and it is a list of scopes.**
`ModelSurface { current, scopes }` (`src/backend/types.rs`) answers what an
**Agent Client** can offer and where each axis may change. A `ModelScope` carries a
`title`, a `ChangeScope` (`ThisTurn` / `NewThread` / `ResumeOnly`), a `PickKind`
(`Model` / `Provider` / `DrillDown`), its options, an optional note and an
optional hand-write hint. The pick kind — not the scope's id — says what a click
does, because both clients legitimately name a scope `provider` and mean
different things by it (OpenCode drills down into another page, Codex selects a
thread-level fact). OpenCode maps its existing `GET /provider` tree onto two
levels unchanged, pixels included; a third **Agent Client** (ACP) only has to answer this
description.

**2. Codex's model axis is three facts, not a tree.** A provider is
**thread-level** (`thread/start` for a new thread and `thread/fork` for a fork
take `modelProvider`; `turn/start` has no such field; `thread/resume`'s override
is a cold-load detail and is no longer used by cola — spec #922), a model is
**per turn** (`turn/start.model`, *"for this turn and subsequent turns"*), and
the catalog is **global**. The Codex surface therefore declares exactly two
scopes — 「模型（下一轮可换）」 (`ThisTurn`) and 「Provider（仅新线程）」
(`NewThread`, only on a Pending Session or through `/fork`; no `ResumeOnly`) —
plus the hand-write hint `/model codex/<id>` for an id no list carries.

**3. `GET <base_url>/models` is not part of the Responses API.** `wire_api =
"responses"` promises the response shape, not the platform's model-listing
endpoint. Measured on the development machine: `opencode-go` — a
`wire_api = "responses"` provider — answers **403** on `/models`, while
`api.deepseek.com/v1/models` answers and lists only its own ids
(`deepseek-flash`, `deepseek-v4-pro`; not `deepseek-chat`). OpenCode itself asks
that endpoint only for a handful of provider plugins (copilot, modal, chatgpt,
azure) and otherwise relies on models.dev or the user's own declaration. The
adapter therefore treats a provider's `/models` as **best effort**: it probes
each configured provider once, with that provider's `experimental_bearer_token`,
2 s per probe, and caches the answer per provider and base URL for three minutes
**including failures** (a 403 must not cost 2 s on every open). A provider that
declares only `env_key` is skipped by name — the daemon's environment is not
cola's, so that credential cannot be read. 401, 403, 404, a timeout, an
unreachable host and an undecodable body are distinct one-line notes, never an
error and never a blank section. A card-callback read answers from that cache
only (the callback budget is 2 s; a fresh probe does not fit), so a cold click
shows the providers without their probed models and the next root `/model` open
fills them. The URL comes from the user's own config — Codex's trust domain — but
its scheme is restricted to http(s), only the host is logged, and the token lives
solely in the outbound `Authorization` header: no note, log line, card value or
`Debug` rendering carries it.

**4. The catalog is global and belongs to no provider.** `model/list` names no
provider, and one of its ids may run through whichever `model_providers` entry
the thread names. It is rendered as its own group 「内置目录（任何 provider
可试）」, never under the current provider. That is also why a picker value stays
the neutral `codex/<id>`: it is the reference cola records, not a claim about
which channel runs the model.

**5. A provider is never selected without a model.** A provider row is a
**pair** — `model` + `model_provider` — because moving a thread from
`opencode-go` to `deepseek` while its model stays `gpt-6-luna` sends that name
to DeepSeek and gets a 404. The pair rule is measured, not stylistic, and it
survives spec #922 (2026-10-30); the *source* of a pair is what changed.

The original #906 text named `[profiles.*]` as that source: a profile was a
declaration of `model` + `model_provider`, never a protocol field. That source
is now **removed**: Codex 0.134+ no longer reads `[profiles.*]` or a top-level
`profile = "..."`; `--profile` layers `$CODEX_HOME/<name>.config.toml`. cola
therefore does **not** read `[profiles.*]` as a preset, and a surface that wants
presets must read those config-profile files (or drop presets).

A configured provider with no pair is not offered as a dead option; the note
names it and says how to declare one. cola keeps no second model list of its
own (cc-connect's `ProviderConfig.Models` was considered as the default and
rejected). The pair sources are, in order of reliability: the catalog plus the
config's `model` for the current provider; a config-profile file, if the
surface reads one; a provider's `/models` answer (best effort, not a
declaration); and a hand-typed id.

**6. A provider change was a live operation in #910, and is now removed.**
*(Historical; superseded by spec #922, 2026-10-30. Do not implement this path:
provider is creation-only — `thread/start` for `/new`, `thread/fork` for
`/fork` — and `/model` refuses a different provider on a live thread.)* #910
measured that a `thread/resume` naming a provider only applies when the thread
reloads, and that a peer-held thread cannot change at all:

```text
A resume(deepseek)                  → deepseek      ← a fresh load applies
A resume(opencode-go)  again        → deepseek      ← silently ignored (still loaded)
A unsubscribe + resume(opencode-go) → opencode-go   ← released, then reloaded
B also resumes (a peer holds it)
A unsubscribe + resume(deepseek)    → opencode-go   ← still ignored: the server keeps it loaded
```

Those measurements are why a live provider switch is not offered. The
implementation that followed them — a release → resume → read-back sequence,
a `provider_switch_on_resume` gate, a confirmation and a mirror rollback — is
deleted with this amendment. The new-thread path (`thread/start` with the pair)
is the only survivor; it is described in decision 5 and the 2026-10-30
amendment.

**7. The old symmetric cost is gone with the switch.** While the switch existed,
cola holding a subscription made another client's provider change a silent
no-op. With provider creation-only there is no live provider operation to
collide with; the model path is a turn input and holds no subscription.

**8. Reads stay primary; the broadcast is a bonus.** `thread/settings/updated`
is still consumed — the projection folds its `model`/`modelProvider` into the
ordered fact — but no model switch depends on it: the stable path is
`turn/start.model` plus the read-back. `effort` is folded as well and rendered
by `/status` and the model card, since the pending carries it; a failed
`config/read` degrades to the catalog plus a named note (「读不到 config，
provider / profile 未知」) instead of an empty section.

`keeps_session_selection = false` and the trait's `switch_session_model` no-op
stand unchanged: the model is turn-carried, not a durable server-side switch.
The per-turn model is the chosen stable shape; the thread's provider is
creation-only.

Where #906's prose and the code disagree, the code is the record: the old
provider-scope sketch and the confirmation are historical now, and a bare
provider key is not selectable (the 404 above).

## Amendment (#910, #908): `codex` is a reserved namespace

*(The `[profiles.*]` half of this section is superseded by spec #922,
2026-10-30; the reserved namespace itself still stands.)*

The neutral catalog ref is `codex/<id>` (decision 3), and that ref is what the
`model` scope's catalog rows carry back. The id is therefore **reserved**:
`codex` can never name a real channel, whatever `config/read` declares. A
`[model_providers.codex]` is still visible in the provider scope — a reserved,
non-selectable row saying 「`codex` 是内置目录的保留名，不能在这里选择」 — but it
offers no pair: the pair value would collide with the catalog's own, so a pick
through it could never address the configured channel. Its probe contributes no
`codex/<id>` entries. (A legacy `[profiles.*]` naming it is no longer read, so
there is no preset to degrade; see decision 5.)

The classification is lexical, so it needs **no** `config/read` on the pick
path: a card callback has an ack budget and the text form has none, and a
cached config read would also let one directory's config decide another
directory's classification. Deciding "is `codex` configured?" from such a cache
made a catalog pick on another provider look like a thread-level switch — the
ambiguity this amendment removes.

## Amendment (#910 AC6): a failed switch leaves the session as it was

*(Superseded by spec #922, 2026-10-30: the live switch and its mirror rollback
are removed. This section is kept as history.)*

A switch whose read-back disagrees (or cannot be read) is a **named failure**,
and the session must not be left holding a pair the thread never ran. The
per-turn mirror is restored from the adapter's read-back observation — a
compare-and-set back to the pair the thread actually runs (a newer pick keeps
its own record); when the pair is unknown the mirror claims nothing. The prompt
is still not sent, and the user **re-picks** to retry once the holder is closed
— the unapplied record is not kept as a retry ticket.

## Amendment (#908): the two-page model/provider presentation supersedes #906 AC1

#906 AC1 asked for **one page** that lists `config/read`'s providers alongside
the catalog and the presets. That page became unreadable: every group, every
provider's probe and its notes crowded one card. The owner chose density over
the letter of AC1, so the Codex surface is now **two pages**, one scope each:

- the **model page** (root) carries only what a pick can run without changing
  the channel — the catalog, the config default and the **current provider's**
  probed models — plus a `切换 provider…` drill;
- the **provider page** (one tap away, with the back button) carries every
  configured provider's pairs, the current marker, a failed probe as a short
  tag and pairless providers as disabled rows.

*(Spec #922, 2026-10-30: the profiles row is removed (see decision 5), and on a
live thread the provider page is a read-only current value with a `/new` /
`/fork` note — the provider rows are selectable only on a Pending Session. The
two-page split itself remains.)*

Nothing is lost in the split: every provider `config/read` declares is still
listed (on the page it belongs to), no bare provider is selectable, and the
model page never carries another provider's probe. OpenCode keeps its own
two pages unchanged.

## Amendment (2026-10-30, spec #922): the model rides the next turn; the provider is creation-only

Decision 7 and the #904/#906/#910 amendments left two claims that the last
implementation could not keep:

- that Codex had no durable model switch, so a pick had to ride every prompt;
- that a provider change could be made by releasing the thread and resuming it
  with `modelProvider`.

Source reading at `rust-v0.163.0` / `rust-v0.166.0`, plus the official
app-server documentation, shows both are wrong in the same direction: the
stable model switch is turn-carried, and the provider is a creation-time fact,
not a live setting. The experimental `thread/settings/update` does exist —
Codex's own TUI uses it for `/model` — but it is deliberately **not** adopted
here: it is not in the public app-server API overview, and this repo prefers
the stable `turn/start.model` over an undocumented method.

**1. `/model` on a live thread is a one-shot, next-turn switch.** `/model`
records a pending selection; it does not change the thread's current pair. The
next **new** `turn/start` carries `model` + `effort`, whose overrides the
protocol documents as becoming "the defaults for later turns on the same
thread". The pending is consumed by the **acceptance of the next new turn**:
when `turn/start` returns a turn that was not already active, the override was
applied and the pending is cleared. It is not re-sent after that. A bounded
`thread/read` then updates the current fact; if it disagrees, that is a named
mismatch (a peer may have changed the pair after us), and the pending is
**not** resurrected — re-sending a stale pick would overwrite the peer's later
change. Subsequent prompts do not carry the model again, so a peer's later
change is respected. The pending is cola state, not server state, and it is
**in-memory only**: a restart drops it and the thread's own model remains. This
is deliberate — a persisted pending cannot distinguish an accepted turn whose
response was lost from one that never ran, and auto-resending could overwrite a
peer's later change. The user re-picks after a restart. The same rule covers an
ambiguous submit while the process stays running: a transport failure, timeout
or lost response drops the pending and never retries it, because the server may
have accepted it; only a definite JSON-RPC refusal leaves the pending for a
re-pick.

**2. A steer, a folded turn, or a command does not consume the pending.** A
Supplement uses `turn/steer`, which has no model field; a `turn/start` that is
folded into an active turn does not promise the override applies; a command
such as `/status` or `/compact` starts no model turn. The pending belongs to
the next *new* turn; only that turn's acceptance consumes it.

**3. A Pending Session's pick is a creation fact, not a pending turn.** It
rides `thread/start { model, modelProvider }` on the first message and has no
"next message" state. The card must say "将在会话创建时生效", never the
live-thread label.

**4. The provider is creation-only.** `/new` (a Pending Session) and `/fork`
are the only paths that name a provider; both send the pair to `thread/start`
or `thread/fork`. A live thread's provider never changes. `/model` refuses a
different provider on a live thread and names `/new` / `/fork`; it sends no
resume and no turn. `provider_switch_on_resume`, the confirmation, the
release-all → `thread/resume { modelProvider, model }` → `thread/read` switch,
and its mirror restore/compare-and-set are removed. This is not a repair of the
resume path: 0.166's `thread/resume` applies a provider override only on a cold
load, and a loaded thread with any subscriber keeps its pair and logs a warning
(source, not a public contract). A switch that silently no-ops under a peer is
worse than refusing the action.

**5. The model surface follows the same split.** The model scope is
`ThisTurn` (a one-shot pending); the provider scope is creation-only
(`NewThread`) and is offered only on a Pending Session or through `/fork`. A
live thread's provider page is a read-only current value with a `/new` /
`/fork` note, not selectable rows. `[profiles.*]` is not a preset source:
Codex 0.134+ removed it, and `--profile` layers
`$CODEX_HOME/<name>.config.toml`. A surface that wants presets reads those
files (or drops presets); this amendment does not keep the dead table alive.

**6. Why the stable interface, and what it costs.** `thread/settings/update`
would make `/model` immediate, durable and peer-visible, but it is an
experimental, undocumented method; a client that needs to stay on the public
surface cannot require it. `turn/start.model` is documented and already
becomes the thread default, so the only cost is the window between `/model`
and the next new turn: `/status` and the card must show "current" and
"pending" as two facts, and another client sees the change after that turn,
not at the click. That is the trade this repo accepts. A future migration is
local: implement `switch_session_model` with `thread/settings/update`, set
`keeps_session_selection = true`, and stop sending the per-turn model; the
provider rule is unchanged.

**7. Two reading rules where docs and source disagree.** The official
app-server docs say a forked thread keeps the session id of the root it came
from, while the 0.166 test asserts the fork's own id; and the docs say the
no-subscriber grace is 30 minutes, while 0.166's default config is 60 seconds.
The adapter reads `thread.sessionId` from the response and the live
`thread/status`; it derives neither.

**Why.** Peer attachment makes a thread's provider a shared fact, not a private
setting. A model pick is a per-turn input that already becomes the thread's
default, so a one-shot pending matches the protocol without a mirror; a
provider pick is a different thread lifecycle, so it belongs to `thread/start`
or `thread/fork`. Separating the two removes the resume/rollback machine that
tried to make a provider switch look like a setting.

**Consequences.** `/model` gains a two-fact display (current + pending) and an
honest "next new message" label. A peer sees the model change after the next
new turn, not immediately, and no `thread/settings/updated` is expected from
this path. Provider changes are refused on a live thread and happen only in
`/new` / `/fork`; `/fork` is a separate feature. The #910 provider-switch
implementation, `provider_switch_on_resume`, its tests and its confirmation
copy are superseded; #918/#919/#920 no longer serve a live switch. The
`GLOSSARY.md` Provider Id / Model Surface entries and both user guides follow
in spec #922's tickets.
