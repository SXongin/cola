# Compaction is a first-class card fact: generation-neutral, session-scoped, and never a silent vanish

V2 persists a compaction as a first-class message (`status` running/completed/failed,
`reason` auto|manual, `summary`, `recent`, and the compaction request's own usage), and
V1 states the same event as a `CompactionPart` on a user message plus a paired assistant
message carrying the summary. cola modelled neither: `decode_body` mapped every
unmodelled kind — `compaction` included — to `Other` with one raw part, and both
`MessageRole::Other` and `Part::Other` are dropped by every projection. So a long
session auto-compacted in silence: the Turn Footer's context ratio dropped (ADR-0044
called that "an honest signal" — honest about *that* the context shrank, silent about
*why*), and a **failed** compaction was indistinguishable from a slow one. `/compact`
shared the blind spot: it submitted, answered "Compacting...", and showed no evidence
that a compaction ever ran. We decided to model a compaction as one generation-neutral
fact and render it.

## Context

- **Two spellings, one event.** V2: a message type. V1 (1.18.x): a `CompactionPart
  {auto, overflow?, tail_start_id?}` on the user message, with `summary:true,
  mode:"compaction"` on the paired assistant message. V1 has no `status` field, no usage
  on the compaction and no `failed` event, and its `overflow` is reported by V2 as
  `reason:"auto"`.
- **Nothing Wakes for a compaction.** cola's `WakeSource::Other("compaction")` mapping is
  a constructed test case: OpenCode publishes `synthetic` only for shell/subagent/
  restart/instructions/continue. A compaction reaches cola only through the transcript
  read (ADR-0011).
- **An in-flight compaction has no readable summary.** The projector writes the `running`
  row with `summary:""` and fills it only at `ended`; the live `session.compaction.delta`
  is ephemeral and not projected, so cola can never see a partial summary — only an empty
  running row, then the whole text at once.
- **The delivery gate.** ADR-0074's same-snapshot ending rule decides a terminal only on
  a read that rendered no new part content, and `renders_part` returns `false` for
  `Part::Other` — so today a Turn settles on the very read that carried the compaction,
  and the marker never gets a chance to render.

## Decision

1. **One neutral fact.** The **Session Transcript** gains a typed `Compaction` carrying
   `status` (optional — absent means completed, V1's shape), `reason`, `summary`
   (optional), `usage` (optional) and `error` (optional). V1 decodes its `CompactionPart`
   plus the paired assistant summary message into it, mapping `overflow` onto
   `reason: auto`. That paired assistant message stops being an answer of its Turn: it is
   the compaction's summary, it never feeds the Turn Footer's context figure, and a
   compaction alone never answers a Turn.
2. **It is renderable content.** A newly delivered compaction holds a card's settlement
   exactly as a new text part does — otherwise the marker is decided away on the read
   that carried it. An in-flight one renders a live row that updates in place when it
   ends. The marker takes its ordered place in the card timeline at its server time: a
   completed one a folded panel naming its reason, summary and usage, a failed one a
   visible failure row with the Backend's error.
3. **Session-scoped, exactly once.** A compaction is a fact of the **Session**, not of a
   Turn: one that precedes a Turn's anchor still renders on that Turn's card, from a
   session-level source rather than the Turn window. The **Rendered Cursor** gains a
   `Compaction` kind so a chain whose newest fact is a compaction is consumed, not
   replayed by every restart projection (ADR-0071).
4. **Surface, don't notify.** An auto-compaction that lands while the session is idle
   pushes nothing: it rides the newest live card if there is one, otherwise it waits for
   the next Turn's card, and the **Session Snapshot**'s recent tail carries it either way
   — interleaved between messages, not consuming one of the tail's four slots.
5. **What is not shown.** `recent` (the verbatim tail kept beside the summary) is never
   rendered: it is history the reader already has. A `providerContext` compaction is not
   labelled differently from any other completed one — "checkpoint" is V2's mechanism
   name, not a domain term.
6. **Failures are loud, and passive.** A failed compaction renders a distinct failure row
   with the Backend's error and offers no retry action: `/compact` stays the user's
   trigger, and a compaction is not a Turn, so it has no Turn **Retry** semantics.
   `/compact`'s own reply stays a submit acknowledgement (now in Chinese, like every
   other command reply).

## Considered options

- **Keep `Other` and special-case the renderer on the raw payload**: rejected. The domain
  fact would not exist, so the transcript filters, the cursor, Session Sync's content
  probe and the snapshot tail would each need their own peek.
- **A footer marker only** (flag the context drop): rejected. It cannot express the
  reason, the summary, or a failure.
- **Render `recent`**: rejected; it duplicates the messages already above the row.
- **Distinguish a provider-native checkpoint**: rejected. It changes nothing the user
  must do.
- **Push a notification for an idle auto-compaction**: rejected. It is background context
  management, not an answer to anyone; a chat message per compaction is noise.
- **V1 in a separate change**: rejected once its `CompactionPart` was confirmed — the
  event is generation-neutral, so only the decode is per-generation while rendering, the
  cursor and the snapshot are written once.
- **A retry action on the failure row**: rejected as out of scope.

## Consequences

- `CursorPartKind` gains a variant — a durable sidecar format addition; an older sidecar
  simply has no such frontier.
- The pinning test `unmodelled_message_types_keep_their_raw_payload` repoints to a
  genuinely unknown kind; the assertion that pins compaction to `Other` is replaced.
- V1's summary text stops rendering as ordinary assistant output (today it already shows,
  unlabelled); the two generations start from opposite places.
- A `running` compaction the server never resolves (a crash mid-compaction) leaves a
  permanent ⏳ row, but wedges nothing: settlement is held for the one read that newly
  delivered it, and the next read may decide an ending.
- The card timeline gains a `TimelineKind::Compaction`; a `Receipt`-style plain line does
  not suffice, because the row is a fold.

## Domain note

`GLOSSARY.md` gains **Compaction**（压缩）; "checkpoint" is explicitly not the domain
term.
