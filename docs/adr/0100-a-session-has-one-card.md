# A session has one card, and `/card` is how you read it

> **Supersedes ADR-0028** (the Session Snapshot card). **Amends ADR-0043's
> 2026-09-22 `/card` amendment**: the pull gains a second state.

## Context

ADR-0028 gave every session activation a read-only Session Snapshot card. It was
the first surface to answer "what happened while I was not looking", and it was
built without a renderer: a `SnapshotData` read, a bespoke card builder, and
`SnapshotClaims` — a registry that hosted adopt-time Permission/Question blocks
on the card and rebuilt it when one resolved.

Two things changed the calculus.

**The claim machinery grew a renderer's problems without a renderer.** #945
records five, and four are the second state machine: a rebuild from
`SnapshotQuestionState::new()` wipes a draft typed on a block nobody clicked; the
standalone switch paints from a state read taken outside the card write lock; the
state read and the rebuild do not share one lock; an ack can land after the
request settled. Every one is impossible by construction on a card that has no
live blocks. The fifth (`overflow` inside a root form) exists only because the
card renders an operable question.

**The snapshot is the only separate card path left.** An External Message's card
is a Turn: `start_reply_render` calls `Turn::arm_external_render`, and the
external loop runs `render_and_flush` — the same pass a cola prompt uses
(ADR-0059). Even the snapshot's busy-follow renders through it, as
`ExternalCardLead::Static`. So the same `SnapshotData` is assembled into card
JSON twice, once by `snapshot_card.rs` when idle and once as a static lead when
busy, and the two can drift.

The premise to reject is that a session needs a second card type to report its
state. It needs a *read*. The card is the session's card.

## Decision

A session has one card, the **Session Card**, with two driving modes:

- **A renderer owns it** — a Turn, an external follow, a Wake continuation: it
  streams, and `/card` keeps its ADR-0043 meaning of splitting the chain and
  continuing at the bottom.
- **Nothing owns it**: `/card` reads the session once (`gather_snapshot`) and
  renders that read as a one-shot Session Card — a real `CardSession` in
  `CardState::Idle`, seeded with an `ExternalCardLead::Static` lead and flushed
  once. It writes **no Chain Record**, so it never makes cola the session's card
  owner, and no Wake, reap or ownership verdict may treat it as one.

The idle card is **read-only**. It carries the status line (等待你的确认 >
等待后台任务 > 运行中 > 需要重试 > 空闲), a count-only pointer naming how many
requests wait on the user, and the 最近对话 tail (four messages; a Compaction's
marker sits at its server position and consumes no slot). It renders no operable
block: a Permission/Question always lives on a live card (inline on the Turn it
blocks) or on its own standalone card (ADR-0074). "Idle" and "interactive" are
mutually exclusive — an unanswered request *is* a session state — so a card that
renders one is not an idle card. This is what dissolves #945: there is no live
block on this card to rebuild, claim, or protect from a repaint.

**Taking a session over no longer posts a card.** A first adoption confirms in
one text line carrying the same status summary, and that line is the Topic
Anchor. Only `/card` produces a card.

A second `/card` with nothing in between stamps the previous idle card
`TakenOver` — and only while that card is still the session's current card. A
Turn that ran in between has already superseded it, so stamping it would only add
noise.

When the read says the session is busy, or waiting on Background Tasks, `/card`
hands the posted card to the existing external renderer (the busy-follow path)
instead of freezing it, so the run's continuation lands on the card the reader
just asked for. Nothing new renders it.

## Consequences

- `src/feishu/snapshot_card.rs`, `src/bridge/snapshot.rs`'s build half and
  `src/bridge/snapshot_claims.rs` go, along with `claimable_pendings`,
  `pending_overflow`, `pending_elsewhere`, `SnapshotQuestionState`,
  `ClaimedSnapshot`, `claim`/`rebuild`/`resolve`/`drop_vanished`, tombstones, and
  `is_already_surfaced`'s snapshot arm. The read survives as **Session Snapshot**.
- `inline_host_session` and `resolve_card_target`'s in-flight arm
  (`src/bridge/pollers.rs`) key on a **live** card, not `Turn::has_card`.
  `has_card` is key presence, and a terminal card stays in `cards` until the next
  Turn replaces it, so an ended card could host a new block today; the idle card
  would have made that the common case.
- An adopted session's pending requests surface through the ordinary sweep
  (`RequestFlow::sweep` lists every known directory's requests): inline on a live
  card, else their own. Nothing is seeded into a host or claimed.
- ADR-0028's suppression heuristic dies with the card: `/card` always answers.
- `build_switched_state_card` moves to `feishu::card::session`; it carries
  ADR-0052's 返回列表 button, which must survive for a switch-list adoption.

## Considered options

- **Keep the snapshot card and give it the turn's repaint gate** (#945's
  proposed direction). It fixes today's five symptoms one at a time and leaves
  two state machines, one of which exists only because a card took on live state
  without a renderer.
- **A thin builder without a `CardSession`.** Cheaper, but keeps a second card
  builder and a separate record of the last idle card, and buys none of the
  shared flush, signature cache or typed-input freeze.
- **A persistent per-session card, patched forever.** Feishu cannot move a
  message; a card buried above the user's own replies stays buried, which is the
  problem `/card` exists to solve.

## Risks

- `/card` becomes the only way to learn an inactive session's state. An adoption
  used to answer it unprompted; now the operator must remember the command. The
  one-line confirmation carries the status summary to soften that.
- The idle card does not refresh itself. If the session moves on elsewhere, the
  card is stale until the next `/card` — which then follows a live run when there
  is one.
- The pending pointer is text, not a link: cola has no message-deep-link
  capability, so it can only say 「见原卡片」. After `/switch forget` and a
  re-adoption in another chat, that card may live in another chat — the same risk
  ADR-0028's `pending_elsewhere` already carried.
- An idle card joins no Chain Record, so a restart does not reap it: it stays in
  the chat as a static message. That is the intended meaning of a read.
